#[test]
fn test_no_color_env_disables_ansi_in_output() {
    let test_file = get_test_file("text/simple.txt");
    if !PathBuf::from(&test_file).exists() {
        return;
    }

    let output = Command::new(get_binary_path())
        .env("NO_COLOR", "1")
        .args(["detect", &test_file])
        .output()
        .expect("Failed to execute detect command");

    assert!(
        output.status.success(),
        "Detect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("\x1b["),
        "Output should not contain ANSI escape sequences when NO_COLOR is set, got: {:?}",
        stdout
    );
}

#[test]
fn test_extract_chunk_size_too_large_error() {
    let (_dir, file_path) = create_temp_file();

    let output = Command::new(get_binary_path())
        .args(["extract", "--chunk-size", "2000000", &file_path])
        .output()
        .expect("Failed to execute extract command");

    assert!(!output.status.success(), "Should fail when chunk size exceeds limit");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("chunk size") || stderr.contains("Chunk size") || stderr.contains("1,000,000"),
        "Error should mention chunk size limit, got: {}",
        stderr
    );
}

#[test]
#[ignore = "requires test_documents/docx/word_image_anchors.docx fixture"]
fn test_extract_images_written_to_output_dir() {
    let test_file = get_test_file("docx/word_image_anchors.docx");
    assert!(
        PathBuf::from(&test_file).exists(),
        "Test fixture not found: {}",
        test_file
    );

    let output_dir = tempdir().expect("Failed to create temp dir");

    let output = Command::new(get_binary_path())
        .args([
            "extract",
            &test_file,
            "--extract-images",
            "true",
            "--content-format",
            "markdown",
            "--output-dir",
            output_dir.path().to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute extract command");

    assert!(
        output.status.success(),
        "Extract command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("image_0."),
        "Expected image reference in markdown, got: {}",
        stdout
    );

    let image_files: Vec<_> = std::fs::read_dir(output_dir.path())
        .expect("Failed to read output dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with("image_")
                && (s.ends_with(".png") || s.ends_with(".jpeg") || s.ends_with(".jpg") || s.ends_with(".webp"))
        })
        .collect();

    assert!(
        !image_files.is_empty(),
        "Expected image files in output dir, but none were written. Markdown was:\n{}",
        stdout
    );

    for entry in &image_files {
        let meta = entry.metadata().expect("Failed to stat image file");
        assert!(
            meta.len() > 0,
            "Image file '{}' is empty",
            entry.file_name().to_string_lossy()
        );
    }
}

#[test]
#[ignore = "requires test_documents/docx/word_image_anchors.docx fixture"]
fn test_extract_images_default_to_cwd_when_no_output_dir() {
    let test_file = get_test_file("docx/word_image_anchors.docx");
    assert!(
        PathBuf::from(&test_file).exists(),
        "Test fixture not found: {}",
        test_file
    );

    let work_dir = tempdir().expect("Failed to create temp dir");

    let output = Command::new(get_binary_path())
        .args([
            "extract",
            &test_file,
            "--extract-images",
            "true",
            "--content-format",
            "markdown",
        ])
        .current_dir(work_dir.path())
        .output()
        .expect("Failed to execute extract command");

    assert!(
        output.status.success(),
        "Extract command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("image_0."), "Expected image reference in markdown");

    let image_files: Vec<_> = std::fs::read_dir(work_dir.path())
        .expect("Failed to read work dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("image_"))
        .collect();

    assert!(
        !image_files.is_empty(),
        "Expected image files in cwd, but none were written"
    );
}

#[test]
fn test_extract_target_dpi_too_high_error() {
    let (_dir, file_path) = create_temp_file();

    let output = Command::new(get_binary_path())
        .args(["extract", "--target-dpi", "5000", &file_path])
        .output()
        .expect("Failed to execute extract command");

    assert!(!output.status.success(), "Should fail when target DPI exceeds limit");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DPI") || stderr.contains("dpi") || stderr.contains("2400") || stderr.contains("Invalid"),
        "Error should mention DPI range, got: {}",
        stderr
    );
}

#[test]
fn test_extract_output_dir_nonexistent_error() {
    let (_dir, file_path) = create_temp_file();

    let output = Command::new(get_binary_path())
        .args([
            "extract",
            &file_path,
            "--output-dir",
            "/nonexistent/directory/that/does/not/exist",
        ])
        .output()
        .expect("Failed to execute extract command");

    assert!(
        !output.status.success(),
        "Extract should fail when --output-dir does not exist"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Output directory not found") || stderr.contains("not found") || stderr.contains("directory"),
        "Error should mention directory not found, got: {}",
        stderr
    );
}
