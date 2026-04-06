use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::error::{InnerErrorCode, MeowError};

pub(crate) async fn calculate_sign(file: &File) -> Result<String, MeowError> {
    crate::meow_flow_log!("sign", "calculate_sign start");
    let mut hasher = md5::Context::new();
    let mut buffer = vec![0; 65536];
    let mut file_handle = file.try_clone().await.map_err(|e| {
        crate::meow_flow_log!("sign", "file.try_clone failed: {}", e);
        MeowError::from_code(
            InnerErrorCode::IoError,
            format!("calculate_sign()->file.try_clone() error: {}", e),
        )
    })?;

    loop {
        let n = file_handle.read(&mut buffer).await.map_err(|e| {
            crate::meow_flow_log!("sign", "file.read failed: {}", e);
            MeowError::from_code(
                InnerErrorCode::IoError,
                format!("calculate_sign()->file_handle.read error: {}", e),
            )
        })?;
        if n == 0 {
            break;
        }
        hasher.consume(&buffer[..n]);
    }

    let digest = hasher.compute();
    crate::meow_flow_log!("sign", "calculate_sign completed");
    Ok(format!("{:x}", digest))
}

#[cfg(test)]
mod tests {
    use super::calculate_sign;
    use tokio::fs::File;

    fn temp_path(case: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        p.push(format!("rusty_cat_sign_unit_{case}_{ts}.bin"));
        p
    }

    #[tokio::test]
    async fn calculate_sign_matches_known_md5_for_non_empty_file() {
        let payload = b"sign-module-non-empty".repeat(1024);
        let expected = format!("{:x}", md5::compute(&payload));
        let path = temp_path("non_empty");
        tokio::fs::write(&path, &payload)
            .await
            .expect("write sign fixture");

        let file = File::open(&path).await.expect("open sign fixture");
        let sign = calculate_sign(&file).await.expect("calculate sign");
        assert_eq!(sign, expected, "sign should equal known MD5");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn calculate_sign_for_empty_file_returns_empty_md5() {
        let path = temp_path("empty");
        tokio::fs::write(&path, b"")
            .await
            .expect("write empty sign fixture");

        let file = File::open(&path).await.expect("open empty sign fixture");
        let sign = calculate_sign(&file).await.expect("calculate empty sign");
        assert_eq!(sign, "d41d8cd98f00b204e9800998ecf8427e");

        let _ = tokio::fs::remove_file(&path).await;
    }
}
