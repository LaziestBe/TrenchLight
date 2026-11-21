use anyhow::{anyhow, bail, Context, Result};
use indicatif::ProgressBar;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util;

const ATTEMPTS: u32 = 3;

pub struct Downloader {
    agent: ureq::Agent,
}

impl Downloader {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(60))
            .user_agent(&format!(
                "{}/{}",
                crate::paths::LAUNCHER_NAME,
                crate::paths::LAUNCHER_VERSION
            ))
            .build();
        Downloader { agent }
    }

    pub fn get_text(&self, url: &str) -> Result<String> {
        let bytes = self.get_bytes(url)?;
        String::from_utf8(bytes).with_context(|| format!("{url} is not utf-8"))
    }

    pub fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let text = self.get_text(url)?;
        serde_json::from_str(&text).with_context(|| format!("parse json from {url}"))
    }

    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 1..=ATTEMPTS {
            match self.agent.get(url).call() {
                Ok(response) => {
                    let mut buf = Vec::new();
                    response
                        .into_reader()
                        .read_to_end(&mut buf)
                        .with_context(|| format!("read body of {url}"))?;
                    return Ok(buf);
                }
                Err(ureq::Error::Status(404, _)) => bail!("not found (404): {url}"),
                Err(error) => {
                    last = Some(anyhow!("{error}"));
                    if attempt < ATTEMPTS {
                        std::thread::sleep(Duration::from_millis(300 * attempt as u64));
                    }
                }
            }
        }
        Err(last.unwrap()).with_context(|| format!("GET {url}"))
    }

    /// Downloads `url` to `dest` unless the file is already valid.
    /// Returns true when bytes were actually transferred.
    pub fn download(
        &self,
        url: &str,
        dest: &Path,
        sha1: Option<&str>,
        size: Option<u64>,
    ) -> Result<bool> {
        if dest.exists() && self.is_valid(dest, sha1, size)? {
            return Ok(false);
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut last: Option<anyhow::Error> = None;
        for attempt in 1..=ATTEMPTS {
            match self.try_download(url, dest, sha1) {
                Ok(()) => return Ok(true),
                Err(error) => {
                    let _ = fs::remove_file(dest);
                    last = Some(error);
                    if attempt < ATTEMPTS {
                        std::thread::sleep(Duration::from_millis(400 * attempt as u64));
                    }
                }
            }
        }
        Err(last.unwrap()).with_context(|| format!("download {url} -> {}", dest.display()))
    }

    /// Same as `download`, but advances a count-based progress bar.
    pub fn download_counted(
        &self,
        url: &str,
        dest: &Path,
        sha1: Option<&str>,
        size: Option<u64>,
        bar: &ProgressBar,
    ) -> Result<bool> {
        let fetched = self.download(url, dest, sha1, size)?;
        bar.inc(1);
        Ok(fetched)
    }

    fn is_valid(&self, dest: &Path, sha1: Option<&str>, size: Option<u64>) -> Result<bool> {
        if let Some(expected) = sha1 {
            let actual = util::sha1_file(dest)?;
            return Ok(actual.eq_ignore_ascii_case(expected));
        }
        if let Some(expected) = size {
            return Ok(fs::metadata(dest)?.len() == expected);
        }
        Ok(true)
    }

    fn try_download(&self, url: &str, dest: &Path, sha1: Option<&str>) -> Result<()> {
        let response = match self.agent.get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(code, _)) => bail!("HTTP {code} for {url}"),
            Err(error) => return Err(anyhow!("{error}")),
        };

        let mut tmp = dest.as_os_str().to_os_string();
        tmp.push(".part");
        let tmp = PathBuf::from(tmp);

        let mut reader = response.into_reader();
        let mut file = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        std::io::copy(&mut reader, &mut file)?;
        file.flush()?;
        drop(file);

        if let Some(expected) = sha1 {
            let actual = util::sha1_file(&tmp)?;
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = fs::remove_file(&tmp);
                bail!("sha1 mismatch for {url}: expected {expected}, got {actual}");
            }
        }

        fs::rename(&tmp, dest).with_context(|| format!("move to {}", dest.display()))?;
        Ok(())
    }
}
