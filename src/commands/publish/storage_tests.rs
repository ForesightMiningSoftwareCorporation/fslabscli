#[cfg(test)]
mod tests {
    use super::super::create_s3_client;
    use crate::commands::check_workspace::binary::BinaryStore;
    use tokio::time::{Duration, sleep};

    // Test constants for MinIO (S3-compatible)
    // Read from environment or use defaults
    fn get_minio_endpoint() -> String {
        std::env::var("S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9000".to_string())
    }
    const MINIO_ACCESS_KEY: &str = "minioadmin";
    const MINIO_SECRET_KEY: &str = "minioadmin";
    const MINIO_REGION: &str = "us-east-1";
    const TEST_BUCKET: &str = "test-bucket";

    // Test constants for Azurite (Azure Blob Storage emulator)
    const AZURITE_ACCOUNT: &str = "devstoreaccount1";
    const AZURITE_KEY: &str =
        "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";
    const AZURITE_ENDPOINT: &str = "http://127.0.0.1:10000/devstoreaccount1";
    const TEST_CONTAINER: &str = "test-container";

    /// Helper to wait for MinIO to be ready
    async fn wait_for_minio() {
        let endpoint = get_minio_endpoint();
        for _ in 0..30 {
            if let Ok(response) = reqwest::get(format!("{}/minio/health/live", endpoint)).await
                && response.status().is_success()
            {
                // Give it an extra second to fully initialize
                sleep(Duration::from_secs(1)).await;
                return;
            }
            sleep(Duration::from_secs(1)).await;
        }
        panic!(
            "MinIO did not become ready in time at endpoint: {}",
            endpoint
        );
    }

    /// Helper to wait for Azurite to be ready
    async fn wait_for_azurite() {
        for _ in 0..30 {
            if reqwest::get(format!("{}/{}", AZURITE_ENDPOINT, TEST_CONTAINER))
                .await
                .is_ok()
            {
                sleep(Duration::from_secs(1)).await;
                return;
            }
            sleep(Duration::from_secs(1)).await;
        }
        panic!("Azurite did not become ready in time");
    }

    #[tokio::test]
    async fn test_s3_write_read_delete() {
        wait_for_minio().await;

        let op = create_s3_client(
            Some(TEST_BUCKET.to_string()),
            Some(MINIO_REGION.to_string()),
            Some(MINIO_ACCESS_KEY.to_string()),
            Some(MINIO_SECRET_KEY.to_string()),
            Some(get_minio_endpoint()),
        )
        .await
        .expect("Failed to create S3 client");

        let test_path = "test-file.txt";
        let test_content = b"Hello from OpenDAL S3 test!".to_vec();

        // Write
        op.write(test_path, test_content.clone())
            .await
            .expect("Failed to write to S3");

        // Read
        let read_content = op.read(test_path).await.expect("Failed to read from S3");
        assert_eq!(read_content.to_vec(), test_content.to_vec());

        // Stat
        let meta = op.stat(test_path).await.expect("Failed to stat file");
        assert_eq!(meta.content_length(), test_content.len() as u64);

        // Check existence
        let exists = op
            .exists(test_path)
            .await
            .expect("Failed to check existence");
        assert!(exists, "File should exist");

        // Delete
        op.delete(test_path)
            .await
            .expect("Failed to delete from S3");

        // Verify deletion
        let exists = op
            .exists(test_path)
            .await
            .expect("Failed to check existence");
        assert!(!exists, "File should not exist after deletion");
    }

    #[tokio::test]
    async fn test_s3_write_multiple_files() {
        wait_for_minio().await;

        let op = create_s3_client(
            Some(TEST_BUCKET.to_string()),
            Some(MINIO_REGION.to_string()),
            Some(MINIO_ACCESS_KEY.to_string()),
            Some(MINIO_SECRET_KEY.to_string()),
            Some(get_minio_endpoint()),
        )
        .await
        .expect("Failed to create S3 client");

        // Write multiple files with directory structure
        let files = vec![
            ("dir/file1.txt", "Content 1"),
            ("dir/file2.txt", "Content 2"),
            ("dir/subdir/file3.txt", "Content 3"),
        ];

        for (path, content) in &files {
            op.write(path, content.to_string())
                .await
                .unwrap_or_else(|_| panic!("Failed to write {}", path));
        }

        // Verify all files exist and have correct content
        for (path, content) in &files {
            let read_content = op
                .read(path)
                .await
                .unwrap_or_else(|_| panic!("Failed to read {}", path));
            assert_eq!(read_content.to_vec(), content.as_bytes().to_vec());
        }

        // Cleanup
        for (path, _) in &files {
            op.delete(path)
                .await
                .unwrap_or_else(|_| panic!("Failed to delete {}", path));
        }
    }

    #[tokio::test]
    async fn test_s3_error_handling() {
        wait_for_minio().await;

        let op = create_s3_client(
            Some(TEST_BUCKET.to_string()),
            Some(MINIO_REGION.to_string()),
            Some(MINIO_ACCESS_KEY.to_string()),
            Some(MINIO_SECRET_KEY.to_string()),
            Some(get_minio_endpoint()),
        )
        .await
        .expect("Failed to create S3 client");

        // Try to read non-existent file
        let result = op.read("non-existent-file.txt").await;
        assert!(result.is_err(), "Reading non-existent file should fail");

        // Verify file doesn't exist
        let exists = op
            .exists("non-existent-file.txt")
            .await
            .expect("Failed to check existence");
        assert!(!exists, "Non-existent file should not exist");

        // Try to delete non-existent file (should succeed - idempotent)
        let result = op.delete("non-existent-file.txt").await;
        assert!(
            result.is_ok(),
            "Deleting non-existent file should be idempotent"
        );
    }

    #[tokio::test]
    async fn test_s3_large_file() {
        wait_for_minio().await;

        let op = create_s3_client(
            Some(TEST_BUCKET.to_string()),
            Some(MINIO_REGION.to_string()),
            Some(MINIO_ACCESS_KEY.to_string()),
            Some(MINIO_SECRET_KEY.to_string()),
            Some(get_minio_endpoint()),
        )
        .await
        .expect("Failed to create S3 client");

        // Create a 1MB file
        let test_path = "large-file.bin";
        let large_content = vec![0u8; 1024 * 1024]; // 1MB

        // Write
        op.write(test_path, large_content.clone())
            .await
            .expect("Failed to write large file to S3");

        // Read and verify
        let read_content = op
            .read(test_path)
            .await
            .expect("Failed to read large file from S3");
        assert_eq!(read_content.len(), large_content.len());

        // Cleanup
        op.delete(test_path)
            .await
            .expect("Failed to delete large file");
    }

    #[tokio::test]
    async fn test_s3_client_creation_missing_credentials() {
        // Missing bucket
        let result = create_s3_client(
            None,
            Some(MINIO_REGION.to_string()),
            Some(MINIO_ACCESS_KEY.to_string()),
            Some(MINIO_SECRET_KEY.to_string()),
            Some(get_minio_endpoint()),
        )
        .await;
        assert!(result.is_err(), "Should fail without bucket name");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing credentials")
        );

        // Missing region
        let result = create_s3_client(
            Some(TEST_BUCKET.to_string()),
            None,
            Some(MINIO_ACCESS_KEY.to_string()),
            Some(MINIO_SECRET_KEY.to_string()),
            Some(get_minio_endpoint()),
        )
        .await;
        assert!(result.is_err(), "Should fail without region");

        // Missing access key
        let result = create_s3_client(
            Some(TEST_BUCKET.to_string()),
            Some(MINIO_REGION.to_string()),
            None,
            Some(MINIO_SECRET_KEY.to_string()),
            Some(get_minio_endpoint()),
        )
        .await;
        assert!(result.is_err(), "Should fail without access key");

        // Missing secret key
        let result = create_s3_client(
            Some(TEST_BUCKET.to_string()),
            Some(MINIO_REGION.to_string()),
            Some(MINIO_ACCESS_KEY.to_string()),
            None,
            Some(get_minio_endpoint()),
        )
        .await;
        assert!(result.is_err(), "Should fail without secret key");
    }

    #[tokio::test]
    async fn test_azure_blob_write_read_delete() {
        wait_for_azurite().await;

        // Create Azure Blob client
        let store = BinaryStore::new_with_endpoint(
            Some(AZURITE_ACCOUNT.to_string()),
            Some(TEST_CONTAINER.to_string()),
            Some(AZURITE_KEY.to_string()),
            Some(AZURITE_ENDPOINT.to_string()),
        )
        .expect("Failed to create BinaryStore")
        .expect("BinaryStore should be Some");

        let op = store.get_client();
        let test_path = "test-blob.txt";
        let test_content = b"Hello from OpenDAL Azure test!".to_vec();

        // Write
        op.write(test_path, test_content.clone())
            .await
            .expect("Failed to write to Azure");

        // Read
        let read_content = op.read(test_path).await.expect("Failed to read from Azure");
        assert_eq!(read_content.to_vec(), test_content.to_vec());

        // Check existence
        let exists = op
            .exists(test_path)
            .await
            .expect("Failed to check existence");
        assert!(exists, "File should exist");

        // Stat
        let meta = op.stat(test_path).await.expect("Failed to stat blob");
        assert_eq!(meta.content_length(), test_content.len() as u64);

        // Delete
        op.delete(test_path)
            .await
            .expect("Failed to delete from Azure");

        // Verify deletion
        let exists = op
            .exists(test_path)
            .await
            .expect("Failed to check existence after delete");
        assert!(!exists, "File should not exist after deletion");
    }

    #[tokio::test]
    async fn test_azure_blob_multiple_files() {
        wait_for_azurite().await;

        let store = BinaryStore::new_with_endpoint(
            Some(AZURITE_ACCOUNT.to_string()),
            Some(TEST_CONTAINER.to_string()),
            Some(AZURITE_KEY.to_string()),
            Some(AZURITE_ENDPOINT.to_string()),
        )
        .expect("Failed to create BinaryStore")
        .expect("BinaryStore should be Some");

        let op = store.get_client();

        // Write multiple blobs
        let files = vec![
            ("azure/file1.txt", "Azure Content 1"),
            ("azure/file2.txt", "Azure Content 2"),
            ("azure/nested/file3.txt", "Azure Content 3"),
        ];

        for (path, content) in &files {
            op.write(path, content.to_string())
                .await
                .unwrap_or_else(|_| panic!("Failed to write {}", path));
        }

        // Verify all files exist and have correct content
        for (path, content) in &files {
            let read_content = op
                .read(path)
                .await
                .unwrap_or_else(|_| panic!("Failed to read {}", path));
            assert_eq!(read_content.to_vec(), content.as_bytes().to_vec());
        }

        // Cleanup
        for (path, _) in &files {
            op.delete(path)
                .await
                .unwrap_or_else(|_| panic!("Failed to delete {}", path));
        }
    }

    #[tokio::test]
    async fn test_azure_blob_error_handling() {
        wait_for_azurite().await;

        let store = BinaryStore::new_with_endpoint(
            Some(AZURITE_ACCOUNT.to_string()),
            Some(TEST_CONTAINER.to_string()),
            Some(AZURITE_KEY.to_string()),
            Some(AZURITE_ENDPOINT.to_string()),
        )
        .expect("Failed to create BinaryStore")
        .expect("BinaryStore should be Some");

        let op = store.get_client();

        // Try to read non-existent blob
        let result = op.read("non-existent-blob.txt").await;
        assert!(result.is_err(), "Reading non-existent blob should fail");

        // Verify blob doesn't exist
        let exists = op
            .exists("non-existent-blob.txt")
            .await
            .expect("Failed to check existence");
        assert!(!exists, "Non-existent blob should not exist");

        // Try to delete non-existent blob (should succeed - idempotent)
        let result = op.delete("non-existent-blob.txt").await;
        assert!(
            result.is_ok(),
            "Deleting non-existent blob should be idempotent"
        );
    }

    #[tokio::test]
    async fn test_azure_blob_store_creation_missing_credentials() {
        // Missing account
        let result = BinaryStore::new_with_endpoint(
            None,
            Some(TEST_CONTAINER.to_string()),
            Some(AZURITE_KEY.to_string()),
            Some(AZURITE_ENDPOINT.to_string()),
        );
        assert!(result.is_ok(), "Result should be Ok");
        assert!(
            result.unwrap().is_none(),
            "Should return None with missing account"
        );

        // Missing container
        let result = BinaryStore::new_with_endpoint(
            Some(AZURITE_ACCOUNT.to_string()),
            None,
            Some(AZURITE_KEY.to_string()),
            Some(AZURITE_ENDPOINT.to_string()),
        );
        assert!(result.is_ok(), "Result should be Ok");
        assert!(
            result.unwrap().is_none(),
            "Should return None with missing container"
        );

        // Missing key
        let result = BinaryStore::new_with_endpoint(
            Some(AZURITE_ACCOUNT.to_string()),
            Some(TEST_CONTAINER.to_string()),
            None,
            Some(AZURITE_ENDPOINT.to_string()),
        );
        assert!(result.is_ok(), "Result should be Ok");
        assert!(
            result.unwrap().is_none(),
            "Should return None with missing key"
        );
    }

    #[tokio::test]
    async fn test_s3_special_characters_in_path() {
        wait_for_minio().await;

        let op = create_s3_client(
            Some(TEST_BUCKET.to_string()),
            Some(MINIO_REGION.to_string()),
            Some(MINIO_ACCESS_KEY.to_string()),
            Some(MINIO_SECRET_KEY.to_string()),
            Some(get_minio_endpoint()),
        )
        .await
        .expect("Failed to create S3 client");

        // Test with spaces and special chars in filename
        let test_path = "dir with spaces/file-with-dashes_and_underscores.txt";
        let test_content = b"Special path test".to_vec();

        op.write(test_path, test_content.clone())
            .await
            .expect("Failed to write file with special chars");

        let read_content = op
            .read(test_path)
            .await
            .expect("Failed to read file with special chars");
        assert_eq!(read_content.to_vec(), test_content.to_vec());

        op.delete(test_path)
            .await
            .expect("Failed to delete file with special chars");
    }

    #[tokio::test]
    async fn test_azure_blob_large_file() {
        wait_for_azurite().await;

        let store = BinaryStore::new_with_endpoint(
            Some(AZURITE_ACCOUNT.to_string()),
            Some(TEST_CONTAINER.to_string()),
            Some(AZURITE_KEY.to_string()),
            Some(AZURITE_ENDPOINT.to_string()),
        )
        .expect("Failed to create BinaryStore")
        .expect("BinaryStore should be Some");

        let op = store.get_client();

        // Create a 1MB blob
        let test_path = "large-blob.bin";
        let large_content = vec![1u8; 1024 * 1024]; // 1MB of ones

        // Write
        op.write(test_path, large_content.clone())
            .await
            .expect("Failed to write large blob");

        // Read and verify
        let read_content = op.read(test_path).await.expect("Failed to read large blob");
        assert_eq!(read_content.len(), large_content.len());

        // Cleanup
        op.delete(test_path)
            .await
            .expect("Failed to delete large blob");
    }
}
