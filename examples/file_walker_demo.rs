use codesearch::FileWalker;
use std::env;

fn main() {
    // Get directory from command line or use current directory
    let path = env::args().nth(1).unwrap_or_else(|| ".".to_string());

    println!("Walking directory: {}", path);
    println!();

    let walker = FileWalker::new(&path);

    match walker.walk() {
        Ok((files, stats)) => {
            println!("\n=== File Discovery Results ===\n");
            println!("Total files discovered: {}", stats.total_files);
            println!("Indexable files: {}", stats.indexable_files);
            println!("Skipped (binary/ignored): {}", stats.skipped_binary);
            println!("Total size: {:.2} MB", stats.total_size_mb());
            println!();

            // Show files by language
            if !stats.files_by_language.is_empty() {
                println!("Files by language:");
                let mut langs: Vec<_> = stats.files_by_language.iter().collect();
                langs.sort_by(|a, b| b.1.cmp(a.1));
                for (lang, count) in langs {
                    println!("  {:15} {:6} files", format!("{}:", lang.name()), count);
                }
            }

            // Show first 10 files
            println!("\nFirst 10 indexable files:");
            for file in files.iter().take(10) {
                println!(
                    "  {:10} {}",
                    format!("[{}]", file.language.name()),
                    file.path.display()
                );
            }

            if files.len() > 10 {
                println!("  ... and {} more", files.len() - 10);
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
