use std::process::Command as SysCommand;

#[derive(Clone)]
pub struct Song {
    pub name: String,
    pub path: String,
}

pub struct DaemonState {
    pub queue: Vec<Song>,
    pub current: Option<Song>,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            current: None,
        }
    }

    pub fn play(&mut self, name: String) {
        let path = crate::paths::song_path(&name);

        self.queue.insert(0, Song { name, path });
    }

    pub fn add(&mut self, name: String) {
        let path = crate::paths::song_path(&name);

        self.queue.push(Song { name, path });
    }

    pub fn delete(&mut self, name: String) {
        let path = crate::paths::song_path(&name);

        let _ = std::fs::remove_file(&path);

        self.queue.retain(|s| s.name != name);
    }

    pub fn init(&mut self, name: String) {
        let path = crate::paths::song_path(&name);

        let result = SysCommand::new("yt-dlp")
            .args([
                "-x",
                "--audio-format",
                "mp3",
                "-o",
                &path,
                &format!("ytsearch1:{}", name),
            ])
            .output();

        if result.is_err() {
            eprintln!("INIT failed: yt-dlp not available or download error");
            return;
        }

        self.queue.push(Song {
            name,
            path,
        });
    }
}
