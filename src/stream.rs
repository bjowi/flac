use nom;
use nom::{Err, IResult};

use metadata;
use frame;
use subframe;

use metadata::{Metadata, StreamInfo, metadata_parser};
use frame::{frame_parser, Frame};
use utility::{ErrorKind, ByteStream, ReadStream, StreamProducer};

use std::io;
use std::usize;
use std::fs::File;

enum ParserState {
  Marker,
  Metadata,
  Frame,
}

pub struct Stream {
  info: StreamInfo,
  pub metadata: Vec<Metadata>,
  frames: Vec<Frame>,
  state: ParserState,
  output: Vec<i32>,
  frame_index: usize,
}

named!(pub stream_parser <&[u8], Stream>,
  chain!(
    blocks: metadata_parser ~
    frames: many1!(apply!(frame_parser, &blocks.0)),
    move|| {
      Stream {
        info: blocks.0,
        metadata: blocks.1,
        frames: frames,
        state: ParserState::Marker,
        output: Vec::new(),
        frame_index: 0,
      }
    }
  )
);

impl Stream {
  pub fn new() -> Stream {
    Stream {
      info: StreamInfo::new(),
      metadata: Vec::new(),
      frames: Vec::new(),
      state: ParserState::Marker,
      output: Vec::new(),
      frame_index: 0,
    }
  }

  pub fn info(&self) -> StreamInfo {
    self.info
  }

  pub fn from_file(filename: &str) -> io::Result<Stream> {
    File::open(filename).and_then(|file| {
      let mut producer = ReadStream::new(file);
      let error_str    = format!("parser: couldn't parse the given file {}",
                                 filename);

      Stream::from_stream_producer(&mut producer, &error_str)
    })
  }

  pub fn from_buffer(buffer: &[u8]) -> io::Result<Stream> {
    let mut producer = ByteStream::new(buffer);
    let error_str    = "parser: couldn't parse the buffer";

    Stream::from_stream_producer(&mut producer, error_str)
  }

  fn from_stream_producer<P>(producer: &mut P, error_str: &str)
                             -> io::Result<Stream>
   where P: StreamProducer {
    let mut is_error = false;
    let mut stream   = Stream {
      info: StreamInfo::new(),
      metadata: Vec::new(),
      frames: Vec::new(),
      state: ParserState::Marker,
      output: Vec::new(),
      frame_index: 0,
    };

    loop {
      match stream.handle(producer) {
        Ok(_)                         => break,
        Err(ErrorKind::EndOfInput)    => break,
        Err(ErrorKind::Consumed(_))   => continue,
        Err(ErrorKind::Incomplete(_)) => continue,
        Err(_)                        => {
          is_error = true;

          break;
        }
      }
    }

    if !is_error {
      let channels    = stream.info.channels as usize;
      let block_size  = stream.info.max_block_size as usize;
      let output_size = block_size * channels;

      stream.output.reserve_exact(output_size);

      unsafe { stream.output.set_len(output_size) }

      Ok(stream)
    } else {
      Err(io::Error::new(io::ErrorKind::InvalidData, error_str))
    }
  }

  pub fn iter(&mut self) -> Iter {
    Iter::new(self)
  }

  fn next_frame<'a>(&'a mut self) -> Option<&'a [i32]> {
    if self.frames.is_empty() || self.frame_index >= self.frames.len() {
      None
    } else {
      let frame       = &self.frames[self.frame_index];
      let channels    = frame.header.channels as usize;
      let block_size  = frame.header.block_size as usize;
      let mut channel = 0;

      for subframe in &frame.subframes[0..channels] {
        let start  = channel * block_size;
        let end    = (channel + 1) * block_size;
        let output = &mut self.output[start..end];

        subframe::decode(&subframe, output);

        channel += 1;
      }

      frame::decode(frame.header.channel_assignment, &mut self.output);

      self.frame_index += 1;

      Some(&self.output[0..(block_size * channels)])
    }
  }

  fn handle_marker<'a>(&mut self, input: &'a [u8]) -> IResult<&'a [u8], ()> {
    let kind = nom::ErrorKind::Custom(0);

    match tag!(input, "fLaC") {
      IResult::Done(i, _)    => {
        self.state = ParserState::Metadata;

        IResult::Error(Err::Position(kind, i))
      }
      IResult::Error(_)      => IResult::Error(Err::Code(kind)),
      IResult::Incomplete(n) => IResult::Incomplete(n),
    }
  }

  fn handle_metadata<'a>(&mut self, input: &'a [u8])
                         -> IResult<&'a [u8], ()> {
    let kind = nom::ErrorKind::Custom(1);

    match metadata::block(input) {
      IResult::Done(i, block) => {
        let is_last = block.is_last;

        if let metadata::Data::StreamInfo(info) = block.data {
          self.info = info;
        } else {
          self.metadata.push(block);
        }

        if is_last {
          self.state = ParserState::Frame;
        }

        IResult::Error(Err::Position(kind, i))
      }
      IResult::Error(_)      => IResult::Error(Err::Code(kind)),
      IResult::Incomplete(n) => IResult::Incomplete(n),
    }
  }

  fn handle_frame<'a>(&mut self, input: &'a [u8]) -> IResult<&'a [u8], ()> {
    let kind = nom::ErrorKind::Custom(2);

    match frame_parser(input, &self.info) {
      IResult::Done(i, frame) => {
        self.frames.push(frame);

        IResult::Error(Err::Position(kind, i))
      }
      IResult::Error(_)      => IResult::Error(Err::Code(kind)),
      IResult::Incomplete(n) => IResult::Incomplete(n),
    }
  }

  fn handle<S: StreamProducer>(&mut self, stream: &mut S)
                               -> Result<(), ErrorKind> {
    stream.parse(|input| {
      match self.state {
        ParserState::Marker   => self.handle_marker(input),
        ParserState::Metadata => self.handle_metadata(input),
        ParserState::Frame    => self.handle_frame(input),
      }
    })
  }
}

pub struct Iter<'a> {
  stream: &'a mut Stream,
  channel: usize,
  frame_index: usize,
  block_size: usize,
  sample_index: usize,
  samples_left: u64,
}

impl<'a> Iter<'a> {
  pub fn new(stream: &'a mut Stream) -> Iter<'a> {
    let samples_left = stream.info.total_samples;

    Iter {
      stream: stream,
      channel: 0,
      frame_index: 0,
      block_size: 0,
      sample_index: 0,
      samples_left: samples_left,
    }
  }
}

impl<'a> Iterator for Iter<'a> {
  type Item = i32;

  fn next(&mut self) -> Option<Self::Item> {
    if self.block_size == 0 || self.sample_index == self.block_size {
      if self.stream.next_frame().is_none() {
        return None;
      } else {
        let frame = &self.stream.frames[self.frame_index];

        self.sample_index = 0;
        self.block_size   = frame.header.block_size as usize;
      }
    }

    let channels = self.stream.info.channels as usize;
    let index    = self.sample_index + (self.channel * self.block_size);
    let sample   = self.stream.output[index];

    self.channel      += 1;
    self.samples_left -= 1;

    // Reset current channel
    if self.channel == channels {
      self.channel       = 0;
      self.sample_index += 1;
    }

    Some(sample)
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let samples_left = self.samples_left as usize;
    let max_value    = usize::max_value() as u64;

    // There is a change that samples_left will be larger than a usize since
    // it is a u64. Make the upper bound None when it is.
    if self.samples_left > max_value {
      (samples_left, None)
    } else {
      (samples_left, Some(samples_left))
    }
  }
}
