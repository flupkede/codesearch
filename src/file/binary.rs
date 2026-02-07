use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Check if a file is binary using multiple heuristics
///
/// This function uses several techniques to detect binary files:
/// 1. File extension (known binary extensions)
/// 2. Null byte detection (most reliable for true binary files)
/// 3. Non-printable character ratio (for text files with some binary data)
/// 4. UTF-8 validity (text files should be valid UTF-8)
pub fn is_binary_file(path: &Path) -> bool {
    // First check: known binary extensions
    if is_binary_by_extension(path) {
        return true;
    }

    // Second check: read file content and analyze
    is_binary_by_content(path)
}

/// Check if file has a known binary extension
fn is_binary_by_extension(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        matches!(
            ext.to_lowercase().as_str(),
            // Executables and libraries
            "exe" | "dll" | "so" | "dylib" | "a" | "o" | "lib" | "bin"
            // Archives
            | "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "tgz"
            // Images
            | "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "svg" | "webp"
            // Videos
            | "mp4" | "avi" | "mov" | "wmv" | "flv" | "mkv" | "webm"
            // Audio
            | "mp3" | "wav" | "ogg" | "flac" | "aac" | "wma"
            // Documents (binary formats)
            | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx"
            // Other binary formats
            | "wasm" | "pyc" | "class" | "jar" | "war"
            // Lock files and minified (not indexable)
            | "lock" | "min.js" | "bundle.js"
        )
    } else {
        false
    }
}

/// Check if file content appears to be binary
fn is_binary_by_content(path: &Path) -> bool {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    // Read first 8KB (sufficient for detection)
    let mut buffer = [0u8; 8192];
    let bytes_read = match file.read(&mut buffer) {
        Ok(n) => n,
        Err(_) => return false,
    };

    // Empty file is not binary
    if bytes_read == 0 {
        return false;
    }

    let data = &buffer[..bytes_read];

    // Check 1: Null bytes are a strong indicator of binary content
    if data.contains(&0) {
        return true;
    }

    // Check 2: Calculate ratio of non-printable characters
    // This includes control characters and non-ASCII bytes
    let non_printable_count = data
        .iter()
        .filter(|&&b| !is_printable_or_whitespace(b))
        .count();

    let non_printable_ratio = non_printable_count as f64 / bytes_read as f64;

    // If more than 30% of characters are non-printable, it's binary
    // UNLESS it's valid UTF-8 with a lower threshold
    if non_printable_ratio > 0.30 {
        // Check if it's valid UTF-8 - if so, it might be text with Unicode
        if std::str::from_utf8(data).is_err() {
            return true;
        }
        // Valid UTF-8 but lots of non-ASCII - check if it's reasonable
        // If >80% non-printable ASCII, it's likely binary even if valid UTF-8
        if non_printable_ratio > 0.80 {
            return true;
        }
    }

    // Passed all checks, likely a text file
    false
}

/// Check if a byte is printable or common whitespace
#[inline]
fn is_printable_or_whitespace(byte: u8) -> bool {
    // Printable ASCII: 0x20 (space) to 0x7E (~)
    // Common whitespace: tab (0x09), newline (0x0A), carriage return (0x0D)
    matches!(byte, 0x09 | 0x0A | 0x0D | 0x20..=0x7E)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_binary_by_extension() {
        assert!(is_binary_by_extension(Path::new("test.exe")));
        assert!(is_binary_by_extension(Path::new("libfoo.so")));
        assert!(is_binary_by_extension(Path::new("image.png")));
        assert!(is_binary_by_extension(Path::new("archive.zip")));
        assert!(is_binary_by_extension(Path::new("video.mp4")));
        assert!(!is_binary_by_extension(Path::new("main.rs")));
        assert!(!is_binary_by_extension(Path::new("README.md")));
    }

    #[test]
    fn test_text_file_detection() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        let mut file = File::create(&file_path).unwrap();
        writeln!(file, "This is a text file").unwrap();
        writeln!(file, "with multiple lines").unwrap();
        drop(file);

        assert!(!is_binary_by_content(&file_path));
    }

    #[test]
    fn test_binary_file_detection() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.bin");
        let mut file = File::create(&file_path).unwrap();
        // Write binary data with null bytes
        file.write_all(&[0x00, 0x01, 0x02, 0x03, 0xFF]).unwrap();
        drop(file);

        assert!(is_binary_by_content(&file_path));
    }

    #[test]
    fn test_non_printable_ratio() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.dat");
        let mut file = File::create(&file_path).unwrap();
        // Write mostly non-printable characters (but no nulls)
        let data: Vec<u8> = (0x01..=0x08).cycle().take(1000).collect();
        file.write_all(&data).unwrap();
        drop(file);

        assert!(is_binary_by_content(&file_path));
    }

    #[test]
    fn test_utf8_validity() {
        let dir = TempDir::new().unwrap();

        // Valid UTF-8
        let valid_path = dir.path().join("valid.txt");
        fs::write(&valid_path, "Hello, 世界!").unwrap();
        assert!(!is_binary_by_content(&valid_path));

        // Invalid UTF-8
        let invalid_path = dir.path().join("invalid.txt");
        fs::write(&invalid_path, &[0xFF, 0xFE, 0xFD]).unwrap();
        assert!(is_binary_by_content(&invalid_path));
    }

    #[test]
    fn test_printable_or_whitespace() {
        assert!(is_printable_or_whitespace(b' ')); // space
        assert!(is_printable_or_whitespace(b'\t')); // tab
        assert!(is_printable_or_whitespace(b'\n')); // newline
        assert!(is_printable_or_whitespace(b'\r')); // carriage return
        assert!(is_printable_or_whitespace(b'A'));
        assert!(is_printable_or_whitespace(b'z'));
        assert!(is_printable_or_whitespace(b'0'));
        assert!(!is_printable_or_whitespace(0x00)); // null
        assert!(!is_printable_or_whitespace(0x01)); // control char
        assert!(!is_printable_or_whitespace(0xFF)); // non-ASCII
    }
}
