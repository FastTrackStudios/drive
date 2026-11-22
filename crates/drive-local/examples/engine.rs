//! How an audio engine gets its session's media from Drive, and how long
//! each step takes on this machine.
//!
//! ```text
//! cargo run -p drive-local --example engine -- [--dir AGENT_DIR] TARGET…
//! ```

use std::time::Instant;

use drive_local::{Client, Event, How, Warm};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let dir = match args.iter().position(|a| a == "--dir") {
        Some(i) => {
            let d = args.remove(i + 1);
            args.remove(i);
            d.into()
        }
        None => drive_local::agent_dir(),
    };
    let t = Instant::now();
    let drive = Client::connect_in(&dir, "engine-example")?;
    println!("connect: {:?}", t.elapsed());

    // The whole media list in one call: what is here answers at once.
    let t = Instant::now();
    let files = drive.resolve_many(&args)?;
    println!("resolve_many ({} files): {:?}", files.len(), t.elapsed());
    for r in &files {
        match (&r.file, &r.error) {
            (Some(f), _) => println!("  {:?} {} {}", f.how, f.size, f.path.display()),
            (None, Some(e)) => println!("  {}: {e}", r.target),
            _ => {}
        }
    }

    // One file again: a hit, now that it is here.
    if let Some(first) = args.first() {
        let t = Instant::now();
        let mut n = 0;
        while n < 1000 {
            let f = drive.open(first)?;
            assert!(matches!(f.how, How::Local | How::Cached));
            n += 1;
        }
        println!("open (hit), mean of 1000: {:?}", t.elapsed() / 1000);
    }

    // Warm the rest of a folder in the background, following its progress.
    if let Ok(folder) = std::env::var("WARM") {
        let t = Instant::now();
        let job = drive.warm(Warm {
            targets: vec![folder],
            ..Default::default()
        })?;
        println!(
            "warm: {} files, {} bytes, {} here",
            job.started.files, job.started.bytes, job.started.here
        );
        while let Some(ev) = job.recv() {
            if let Event::Done(p) = ev {
                println!(
                    "warm done: {}/{} files, {} bytes in {:?}",
                    p.files_done,
                    p.files,
                    p.bytes_done,
                    t.elapsed()
                );
                break;
            }
        }
    }

    // Reload when a file gets a new version.
    if std::env::var("WATCH").is_ok()
        && let Some(first) = args.first()
    {
        let w = drive.watch(first)?;
        println!("watching {first}");
        if let Some(Event::Changed { size, .. }) = w.recv() {
            println!("new version ({size} bytes): reload");
        }
    }
    Ok(())
}
