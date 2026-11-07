use super::*;
use crate::fixtures::STREAMS as FIXTURES;

/// Decode `chunks` in order, then finish.
fn decode<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Result<Vec<SseFrame>, SseError> {
    let mut decoder = SseDecoder::default();
    let mut frames = Vec::new();
    for chunk in chunks {
        decoder.feed(chunk, &mut frames)?;
    }
    decoder.finish(&mut frames)?;
    Ok(frames)
}

fn event(name: Option<&str>, data: &str) -> SseFrame {
    SseFrame::Event(SseEvent {
        event: name.map(str::to_owned),
        data: data.to_owned(),
    })
}

/// The cheap fuzz: however a stream is cut into chunks, whichever line
/// ending it uses, and whether a byte-order mark starts it, it decodes to
/// the same frames as in one piece (a mark cut into pieces once cost the
/// first event its name).
#[test]
fn every_split_and_every_line_ending_decodes_the_same() {
    for (name, text) in FIXTURES {
        let whole = decode([text.as_bytes()]).unwrap();
        assert!(whole.len() > 2, "{name}: too few frames");
        let crlf = text.replace('\n', "\r\n");
        let cr = text.replace('\n', "\r");
        let marked = format!("\u{feff}{text}");
        for variant in [text.to_owned(), crlf, cr, marked] {
            let bytes = variant.as_bytes();
            assert_eq!(decode([bytes]).unwrap(), whole, "{name}: one piece");
            for split in 0..=bytes.len() {
                let (head, tail) = bytes.split_at(split);
                assert_eq!(
                    decode([head, tail]).unwrap(),
                    whole,
                    "{name}: split at {split}"
                );
            }
            let bytewise = decode(bytes.chunks(1)).unwrap();
            assert_eq!(bytewise, whole, "{name}: byte by byte");
        }
    }
}

#[test]
fn fixtures_decode_to_their_events() {
    let frames = decode([FIXTURES[0].1.as_bytes()]).unwrap();
    assert_eq!(
        frames.first(),
        Some(&event(
            Some("message_start"),
            r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","model":"claude-sonnet-4-5","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"cache_creation_input_tokens":0,"cache_read_input_tokens":100,"output_tokens":1}}}"#
        ))
    );
    assert_eq!(
        frames.last(),
        Some(&event(Some("message_stop"), r#"{"type":"message_stop"}"#))
    );

    let chat = decode([FIXTURES[3].1.as_bytes()]).unwrap();
    assert_eq!(chat.first(), Some(&SseFrame::Comment));
    assert_eq!(chat.last(), Some(&event(None, "[DONE]")));
}

#[test]
fn fields_follow_the_format() {
    let text = "\u{feff}: keepalive\n\
                event: one\n\
                data: first line\n\
                data:second line\n\
                id: 7\n\
                retry: 100\n\
                whatever: ignored\n\
                \n\
                event: nameless data dropped\n\
                \n\
                data\n\
                \n\
                data: last, without its blank line";
    assert_eq!(
        decode([text.as_bytes()]).unwrap(),
        [
            SseFrame::Comment,
            event(Some("one"), "first line\nsecond line"),
            // A field with no colon is a field with an empty value.
            event(None, ""),
            event(None, "last, without its blank line"),
        ]
    );
}

/// A stream too short to tell whether it starts with a mark still
/// decodes what it has.
#[test]
fn a_stream_shorter_than_a_mark_is_decoded_at_its_end() {
    assert_eq!(decode([b"\xEF\xBB".as_slice()]), Err(SseError::NotUtf8));
    assert_eq!(decode([b"\xEF\xBB\xBF".as_slice()]).unwrap(), []);
    assert_eq!(decode([b"da".as_slice()]).unwrap(), []);
    assert_eq!(
        decode([
            b"\xEF".as_slice(),
            b"\xBB".as_slice(),
            b"\xBFdata".as_slice()
        ])
        .unwrap(),
        [event(None, "")]
    );
}

#[test]
fn a_line_or_an_event_past_the_bound_is_refused() {
    let mut decoder = SseDecoder::new(16);
    let mut frames = Vec::new();
    assert_eq!(
        decoder.feed(b"data: this line goes on and on", &mut frames),
        Err(SseError::TooLarge { limit: 16 })
    );

    let mut decoder = SseDecoder::new(16);
    let refused = decoder
        .feed(b"data: 01234567\n", &mut frames)
        .and_then(|()| decoder.feed(b"data: 01234567\n", &mut frames));
    assert_eq!(refused, Err(SseError::TooLarge { limit: 16 }));

    assert_eq!(
        decode([b"data: \xff\n\n".as_slice()]),
        Err(SseError::NotUtf8)
    );
}

#[test]
fn encoded_events_decode_back() {
    let mut stream = Vec::new();
    stream.extend_from_slice(&encode_event(Some("delta"), "{\"a\":1}").unwrap());
    stream.extend_from_slice(&encode_comment("ping\nwith a break"));
    stream.extend_from_slice(&encode_event(None, "two\nlines\n").unwrap());
    stream.extend_from_slice(&encode_event(None, "").unwrap());
    assert_eq!(
        decode([stream.as_slice()]).unwrap(),
        [
            event(Some("delta"), "{\"a\":1}"),
            SseFrame::Comment,
            event(None, "two\nlines\n"),
            event(None, ""),
        ]
    );
    assert_eq!(
        encode_event(Some("bad\nname"), "x"),
        Err(EncodeError::NameLineBreak)
    );
}
