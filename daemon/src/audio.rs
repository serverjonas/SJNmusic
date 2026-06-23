use log::debug;
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

    pub fn play(&mut self, path: &str) -> Result<(), String> {
        let (_stream, handle) = OutputStream::try_default()
            .map_err(|e| format!("could not open audio output: {e}"))?;
        let sink = Sink::try_new(&handle).map_err(|e| format!("could not create sink: {e}"))?;

        let file = File::open(path).map_err(|e| format!("could not open file: {e}"))?;
        let source =
            Decoder::new(BufReader::new(file)).map_err(|e| format!("decode error: {e}"))?;

        sink.append(source);
        debug!("rodio sink appended for {path}");

        self.sink = Some(sink);
        self._stream = Some(_stream);
        Ok(())
    }

    pub fn is_playing(&self) -> bool {
        match &self.sink {
            Some(s) => !s.empty(),
            None => false,
        }
    }
}
