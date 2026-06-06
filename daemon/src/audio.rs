use rodio::{Decoder, OutputStream, Sink};
use std::fs::File;
use std::io::BufReader;

pub struct AudioEngine {
    sink: Option<Sink>,
    _stream: Option<OutputStream>,
}

impl AudioEngine {
    pub fn new() -> Self {
        Self {
            sink: None,
            _stream: None,
        }
    }

    pub fn play(&mut self, path: &str) {
        let (_stream, handle) = OutputStream::try_default().unwrap();
        let sink = Sink::try_new(&handle).unwrap();

        let file = File::open(path).unwrap();
        let source = Decoder::new(BufReader::new(file)).unwrap();

        sink.append(source);

        self.sink = Some(sink);
        self._stream = Some(_stream);
    }

    pub fn is_playing(&self) -> bool {
        match &self.sink {
            Some(s) => !s.empty(),
            None => false,
        }
    }
}
