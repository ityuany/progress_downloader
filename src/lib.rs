use std::{env, path::Path, sync::Arc, time::Duration};

use backoff::ExponentialBackoff;
use err::ProgressDownloadError;
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressDrawTarget};
use progress_bar_delegate::ProgressBarDelegate;
use tokio::{io::AsyncWriteExt, sync::Semaphore};
use typed_builder::TypedBuilder;

mod err;
mod progress_bar_delegate;

#[derive(Debug, TypedBuilder, Clone)]
pub struct DownloadProgress {
  #[builder(default = Duration::from_millis(2_000))]
  connect_timeout: Duration,

  #[builder(default = Duration::from_secs(60))]
  timeout: Duration,

  // 添加新的配置参数，默认为 512KB
  #[builder(default = 512 * 1024)]
  flush_threshold: usize,

  #[builder(default = 2)]
  max_concurrent: usize,
}

impl DownloadProgress {
  fn backoff(&self) -> ExponentialBackoff {
    ExponentialBackoff {
      // 初始等待 0.5 秒,加快重试速度
      initial_interval: Duration::from_millis(500),
      // 保持 15% 随机波动不变
      randomization_factor: 0.15,
      // 每次增加 1.5 倍,减缓增长速度
      multiplier: 1.5,
      // 最大等待 5 秒,缩短最大等待时间
      max_interval: Duration::from_secs(5),
      // 最多重试 1 分钟
      max_elapsed_time: Some(Duration::from_secs(120)),
      ..Default::default()
    }
  }

  pub async fn download(&self, downloads: Vec<(&str, &str)>) -> Result<(), ProgressDownloadError> {
    let client = reqwest::Client::builder()
      .connect_timeout(self.connect_timeout)
      .pool_max_idle_per_host(0)
      .build()?;

    let mp = indicatif::MultiProgress::new();

    // 创建信号量来控制并发
    let semaphore = Arc::new(Semaphore::new(self.max_concurrent));

    let futures = downloads.into_iter().map(|(url, path)| {
      let sem = semaphore.clone();
      let client = client.clone();
      let mp = mp.clone();

      async move {
        // 获取信号量许可
        let _permit = sem.acquire().await?;
        self.download_with_retry(&client, &mp, url, path).await
      }
    });

    futures::future::try_join_all(futures).await?;
    mp.set_move_cursor(true);
    mp.clear()?;
    println!("🎉 Download completed");

    Ok(())
  }

  fn prepare_progress_bar(&self) -> ProgressBar {
    let progress_bar = ProgressBar::with_draw_target(Some(0), ProgressDrawTarget::stdout());
    progress_bar.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] {bar:25.green/white.dim} {bytes}/{total_bytes} {wide_msg:.dim}",
            )
            .unwrap()
            .progress_chars("━━"),
        );
    progress_bar
  }

  async fn send(
    &self,
    client: &reqwest::Client,
    url: &str,
    downloaded_size: u64,
  ) -> Result<reqwest::Response, ProgressDownloadError> {
    let request = client
      .get(url)
      .header("Range", format!("bytes={}-", downloaded_size))
      .timeout(self.timeout);

    let response = request.send().await?;

    Ok(response)
  }

  async fn operation<P: AsRef<Path>>(
    &self,
    client: &reqwest::Client,
    progress_bar: &indicatif::ProgressBar,
    temp_file: P,
    url: &str,
  ) -> Result<(), ProgressDownloadError> {
    let temp_file = temp_file.as_ref();
    let downloaded_size = temp_file.metadata().map(|item| item.len()).unwrap_or(0);

    let response = self.send(client, url, downloaded_size).await?;
    let supports_resume = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let remaining_size = response.content_length().unwrap_or(0);

    let should_resume = supports_resume && downloaded_size > 0;

    let file = tokio::fs::OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(!should_resume)
      .append(should_resume)
      .open(temp_file)
      .await?;

    let mut delegate = ProgressBarDelegate::builder()
      .progress_bar(progress_bar)
      .downloaded_size(downloaded_size)
      .remaining_size(remaining_size)
      .url(url.to_string())
      .build();

    delegate.init_progress();

    let mut writer = tokio::io::BufWriter::with_capacity(1024 * 1024, file);

    let stream = response.bytes_stream();

    tokio::pin!(stream);

    while let Some(chunk) = tokio::time::timeout(Duration::from_millis(500), stream.next())
      .await?
      .transpose()?
    {
      delegate.update_progress(chunk.len());

      writer.write_all(&chunk).await?;

      // 减少刷新频率，提高性能
      if writer.buffer().len() >= self.flush_threshold {
        writer.flush().await?;
      }
    }

    // 确保所有数据都写入
    writer.flush().await?;

    Ok(())
  }

  async fn download_with_retry(
    &self,
    client: &reqwest::Client,
    mp: &indicatif::MultiProgress,
    url: &str,
    path: &str,
  ) -> Result<(), ProgressDownloadError> {
    let temp_dir = env::temp_dir();
    let temp_file = temp_dir.join(path);

    let progress_bar = self.prepare_progress_bar();
    let progress_bar = mp.add(progress_bar);

    backoff::future::retry(self.backoff(), || async {
      self
        .operation(client, &progress_bar, &temp_file, url)
        .await
        .map_err(ProgressDownloadError::into_backoff_err)
    })
    .await?;

    progress_bar.finish_with_message(format!("Downloaded {} to {}", url, path));

    tokio::fs::rename(&temp_file, path).await?;

    Ok(())
  }
}
