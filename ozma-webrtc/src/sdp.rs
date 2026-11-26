// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! SDP helpers shared by the signaling path.
//!
//! `webrtc-rs` gathers host candidates asynchronously after the local
//! description is installed. Like the Python GStreamer worker, the helper
//! waits a bounded time, then appends the candidates it actually gathered to
//! the answer so a peer that does not trickle can still connect. Nothing here
//! invents a candidate.

/// Split an SDP into its preamble and per-`m=` sections.
///
/// Every section starts with its `m=` line. CRLF and LF input behave the same.
pub fn split_media_sections(sdp: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut preamble = Vec::new();
    let mut sections: Vec<Vec<String>> = Vec::new();
    for raw_line in sdp.lines() {
        let line = raw_line.trim_end_matches('\r').to_owned();
        if line.starts_with("m=") {
            sections.push(vec![line]);
        } else if let Some(section) = sections.last_mut() {
            section.push(line);
        } else {
            preamble.push(line);
        }
    }
    (preamble, sections)
}

/// Return `(mline_index, candidate)` for every `a=candidate:` line.
pub fn remote_ice_candidates(sdp: &str) -> Vec<(usize, String)> {
    let (_, sections) = split_media_sections(sdp);
    let mut candidates = Vec::new();
    for (index, lines) in sections.iter().enumerate() {
        for line in lines {
            let Some(value) = line.strip_prefix("a=") else {
                continue;
            };
            if !value.starts_with("candidate:") {
                continue;
            }
            let value = value.trim();
            if !value.is_empty() && !value.starts_with("candidate:end-of-candidates") {
                candidates.push((index, value.to_owned()));
            }
        }
    }
    candidates
}

/// Append `a=candidate` lines to their matching `m=` sections.
///
/// Candidate values may include or omit the `candidate:` prefix. Duplicates
/// are skipped and `a=end-of-candidates` stays last in the section. An SDP with
/// no media section is returned unchanged.
pub fn append_ice_candidates(sdp: &str, candidates: &[(u16, String)]) -> String {
    if candidates.is_empty() {
        return sdp.to_owned();
    }
    let (preamble, sections) = split_media_sections(sdp);
    if sections.is_empty() {
        return sdp.to_owned();
    }

    let mut by_index: Vec<Vec<String>> = vec![Vec::new(); sections.len()];
    for (index, candidate) in candidates {
        let value = candidate.trim();
        if value.is_empty() {
            continue;
        }
        let value = if let Some(stripped) = value.strip_prefix("a=candidate:") {
            format!("candidate:{stripped}")
        } else if value.starts_with("candidate:") {
            value.to_owned()
        } else {
            format!("candidate:{value}")
        };
        if let Some(slot) = by_index.get_mut(*index as usize) {
            slot.push(value);
        }
    }

    let mut lines = preamble;
    for (index, section) in sections.iter().enumerate() {
        let existing: Vec<String> = section
            .iter()
            .filter(|line| line.starts_with("a=candidate:"))
            .cloned()
            .collect();
        let additions: Vec<String> = by_index[index]
            .iter()
            .filter(|candidate| !existing.contains(&format!("a={candidate}")))
            .map(|candidate| format!("a={candidate}"))
            .collect();
        if additions.is_empty() {
            lines.extend(section.iter().cloned());
            continue;
        }
        let position = section
            .iter()
            .position(|line| line.starts_with("a=end-of-candidates"))
            .unwrap_or(section.len());
        lines.extend(section[..position].iter().cloned());
        lines.extend(additions);
        lines.extend(section[position..].iter().cloned());
    }
    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    out
}

/// The peer's payload type for `encoding` on the first matching `m=<media>`
/// section.
///
/// The answer must reuse the offered payload type: `webrtc-rs` otherwise sends
/// with a media-engine default (for example 102) while a GStreamer receiver
/// only knows the payload type in its own offer (for example 96). This mirrors
/// `gst_webrtc_worker.py`'s `offered_payload_type`.
pub fn offered_payload_type(sdp: &str, media: &str, encoding: &str) -> Option<u8> {
    let (_, sections) = split_media_sections(sdp);
    for section in &sections {
        let header: Vec<&str> = section[0].split_whitespace().collect();
        if header.len() < 4 || header[0] != format!("m={media}") {
            continue;
        }
        let offered: Vec<&str> = header[3..].to_vec();
        for line in &section[1..] {
            let Some(rest) = line.strip_prefix("a=rtpmap:") else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let Some(pt) = parts.next() else { continue };
            let Ok(payload_type) = pt.parse::<u8>() else {
                continue;
            };
            let Some(encoding_text) = parts.next() else {
                continue;
            };
            if offered.contains(&pt)
                && encoding_text
                    .split('/')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case(encoding))
            {
                return Some(payload_type);
            }
        }
        // Only the first matching media section matters.
        return None;
    }
    None
}

/// The ICE type token of a candidate, when it carries a recognised one.
pub fn candidate_type(candidate: &str) -> Option<&'static str> {
    let value = candidate
        .trim()
        .strip_prefix("a=")
        .unwrap_or(candidate.trim());
    let mut parts = value.split_whitespace();
    while let Some(part) = parts.next() {
        if part == "typ" {
            return match parts.next()?.to_ascii_lowercase().as_str() {
                "host" => Some("host"),
                "srflx" => Some("srflx"),
                "prflx" => Some("prflx"),
                "relay" => Some("relay"),
                _ => None,
            };
        }
    }
    None
}

/// Count the ICE types in `(mline, candidate)` pairs.
pub fn candidate_type_counts(candidates: &[(usize, String)]) -> Vec<(&'static str, usize)> {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for (_, candidate) in candidates {
        if let Some(kind) = candidate_type(candidate) {
            match counts.iter_mut().find(|(name, _)| *name == kind) {
                Some((_, count)) => *count += 1,
                None => counts.push((kind, 1)),
            }
        }
    }
    counts.sort_by_key(|(name, _)| *name);
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "v=0\r\n\
o=- 1 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
a=mid:video\r\n\
a=rtpmap:102 H264/90000\r\n\
a=sendonly\r\n\
a=end-of-candidates\r\n";

    fn host_candidate(port: u16) -> String {
        format!("candidate:1 1 UDP 2130706431 192.0.2.10 {port} typ host")
    }

    #[test]
    fn splits_preamble_and_media_sections() {
        let (preamble, sections) = split_media_sections(ANSWER);
        assert_eq!(preamble.len(), 4);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0][0], "m=video 9 UDP/TLS/RTP/SAVPF 102");
    }

    #[test]
    fn appends_a_candidate_before_end_of_candidates() {
        let out = append_ice_candidates(ANSWER, &[(0, host_candidate(5000))]);
        let lines: Vec<&str> = out.lines().collect();
        let candidate = lines
            .iter()
            .position(|line| line.starts_with("a=candidate:1"))
            .expect("candidate line");
        let end = lines
            .iter()
            .position(|line| line.starts_with("a=end-of-candidates"))
            .expect("end-of-candidates");
        assert!(candidate < end);
        assert!(out.ends_with("\r\n"));
    }

    #[test]
    fn duplicate_candidates_are_not_added_twice() {
        let candidate = host_candidate(5000);
        let once = append_ice_candidates(ANSWER, &[(0, candidate.clone())]);
        let twice = append_ice_candidates(&once, &[(0, candidate)]);
        assert_eq!(once, twice);
        assert_eq!(
            twice
                .lines()
                .filter(|line| line.starts_with("a=candidate:"))
                .count(),
            1
        );
    }

    #[test]
    fn candidates_go_to_their_own_mline() {
        let sdp = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:v\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 97\r\na=mid:a\r\n";
        let out =
            append_ice_candidates(sdp, &[(0, host_candidate(1000)), (1, host_candidate(2000))]);
        let (_, sections) = split_media_sections(&out);
        assert!(sections[0].iter().any(|line| line.contains(" 1000 ")));
        assert!(!sections[0].iter().any(|line| line.contains(" 2000 ")));
        assert!(sections[1].iter().any(|line| line.contains(" 2000 ")));
    }

    #[test]
    fn candidate_without_prefix_is_normalised() {
        let out = append_ice_candidates(ANSWER, &[(0, "1 1 UDP 1 192.0.2.1 9 typ host".into())]);
        assert!(out.contains("a=candidate:1 1 UDP 1 192.0.2.1 9 typ host"));
    }

    #[test]
    fn extracts_remote_candidates_and_ignores_end_marker() {
        let sdp = format!(
            "m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
a=candidate:1 1 UDP 1 192.0.2.1 5000 typ host\r\n\
a=candidate:end-of-candidates\r\n"
        );
        let candidates = remote_ice_candidates(&sdp);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, 0);
        assert!(candidates[0].1.starts_with("candidate:1"));
    }

    #[test]
    fn extracts_the_offered_h264_payload_type() {
        assert_eq!(offered_payload_type(ANSWER, "video", "H264"), Some(102));
        assert_eq!(offered_payload_type(ANSWER, "video", "VP8"), None);
        assert_eq!(offered_payload_type(ANSWER, "audio", "opus"), None);
        let chrome = "v=0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 103 104\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=rtpmap:103 H264/90000\r\n\
a=rtpmap:104 rtx/90000\r\n";
        assert_eq!(offered_payload_type(chrome, "video", "H264"), Some(103));
        assert_eq!(offered_payload_type(chrome, "audio", "OPUS"), Some(111));
        // A payload type not present in the m= line is not accepted.
        let stale = "m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:103 H264/90000\r\n";
        assert_eq!(offered_payload_type(stale, "video", "H264"), None);
    }

    #[test]
    fn classifies_candidate_types() {
        assert_eq!(
            candidate_type("candidate:1 1 UDP 1 1.2.3.4 5 typ host"),
            Some("host")
        );
        assert_eq!(
            candidate_type("a=candidate:1 1 UDP 1 1.2.3.4 5 typ srflx raddr 0.0.0.0 rport 0"),
            Some("srflx")
        );
        assert_eq!(candidate_type("candidate:1 1 UDP 1 1.2.3.4 5"), None);
        let counts = candidate_type_counts(&[(0, "candidate:1 1 UDP 1 1.2.3.4 5 typ host".into())]);
        assert_eq!(counts, vec![("host", 1)]);
    }
}
