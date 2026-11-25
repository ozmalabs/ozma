// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, deterministic fuzz regression for the Rust SVG/CSS extractors.
//!
//! WGT-082: these are normal integration tests, not a libFuzzer crate.  A
//! seeded LCG generates size-bounded hostile documents; every call must return
//! `Ok` with bounded output or a typed `IrError`, never panic.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ozma-display-ir --test fuzz_extract
//! ```

use ozma_display_ir::{
    extract_from_css, extract_from_svg, parse_svg_path, IrError, VisualExtraction, MAX_TEXT_INPUT,
};

/// Deterministic 64-bit LCG.  No external crates are added for fuzzing.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    fn below(&mut self, limit: usize) -> usize {
        if limit == 0 {
            return 0;
        }
        (self.next_u64() as usize) % limit
    }

    fn text(&mut self, max_chars: usize) -> String {
        const ALPHABET: &[char] = &[
            '<', '>', '/', '{', '}', '"', '\'', '=', ':', ';', '-', '_', '&', '#', '!', '.', ',',
            '%', '(', ')', ' ', '\n', '\t', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'x', 'y', 'z', '0',
            '1', '2', '3', '4', '5', '6', '7', '8', '9', 'é', '漢', '😀',
        ];
        let length = self.below(max_chars + 1);
        let mut output = String::with_capacity(length * 2);
        for _ in 0..length {
            output.push(ALPHABET[self.below(ALPHABET.len())]);
        }
        output
    }
}

fn assert_bounded_extraction(label: &str, seed: u64, text: &str, output: VisualExtraction) {
    let bound = text.len().saturating_add(1);
    assert!(
        output.colors.len() <= bound,
        "{label}: colors unbounded for seed {seed}"
    );
    assert!(
        output.fonts.len() <= bound,
        "{label}: fonts unbounded for seed {seed}"
    );
    assert!(
        output.gradients.len() <= bound,
        "{label}: gradients unbounded for seed {seed}"
    );
    if let Some(ratio) = output.shape.aspect_ratio {
        assert!(
            ratio.is_finite(),
            "{label}: non-finite aspect ratio for seed {seed}"
        );
    }
    if let Some(count) = output.shape.tick_count {
        // A hostile document may ask for a huge tick count; the parser must
        // clamp it, not allocate or emit an unbounded structure.
        assert!(
            count <= 100_000,
            "{label}: tick count {count} is not clamped for seed {seed}"
        );
    }
}

fn assert_no_panic<T, F>(seed: u64, label: &str, call: F) -> T
where
    F: FnOnce() -> T + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(call) {
        Ok(value) => value,
        Err(_) => panic!("{label}: panicked for seed {seed}"),
    }
}

#[test]
fn random_svg_css_and_paths_are_typed_or_bounded() {
    let mut rng = Rng::new(0x5754_4730_3832);
    for seed in 0..400u64 {
        let text = rng.text(4096);

        let svg = assert_no_panic(seed, "extract_from_svg", || extract_from_svg(&text));
        if let Ok(output) = svg {
            assert_bounded_extraction("svg", seed, &text, output);
        }

        let css = assert_no_panic(seed, "extract_from_css", || extract_from_css(&text));
        if let Ok(output) = css {
            assert_bounded_extraction("css", seed, &text, output);
        }

        let path = assert_no_panic(seed, "parse_svg_path", || parse_svg_path(&text));
        if let Ok(stats) = path {
            let bound = text.len() as u32 + 1;
            assert!(stats.commands <= bound, "path commands unbounded");
            assert!(stats.segments <= bound, "path segments unbounded");
            assert!(stats.arcs <= bound, "path arcs unbounded");
        }
    }
}

#[test]
fn oversized_input_is_rejected_before_parsing() {
    let oversized = "x".repeat(MAX_TEXT_INPUT + 1);
    assert!(matches!(
        extract_from_svg(&oversized),
        Err(IrError::InvalidValue { .. })
    ));
    assert!(matches!(
        extract_from_css(&oversized),
        Err(IrError::InvalidValue { .. })
    ));

    let empty = "   \n\t";
    assert!(matches!(
        extract_from_svg(empty),
        Err(IrError::EmptyField("svg"))
    ));
    assert!(matches!(
        extract_from_css(empty),
        Err(IrError::EmptyField("css"))
    ));
}

#[test]
fn unterminated_and_entity_svg_inputs_are_typed_errors() {
    let cases = [
        "<svg viewBox=\"0 0 10 10\"><!ENTITY x \"y\"></svg>",
        "<svg><!-- unterminated",
        "<svg",
        "<svg viewBox=\"0 0 10 10\"><g/></svg><!-- tail",
    ];
    for text in cases {
        let _ = assert_no_panic(0, "svg hostile case", || extract_from_svg(text));
        assert!(
            extract_from_svg(text).is_err(),
            "expected typed error for {text:?}"
        );
    }
}

#[test]
fn deep_svg_nesting_stays_bounded() {
    let mut text = String::from("<svg viewBox=\"0 0 10 10\">");
    for _ in 0..3000 {
        text.push_str("<g>");
    }
    for _ in 0..3000 {
        text.push_str("</g>");
    }
    text.push_str("</svg>");
    assert!(text.len() <= MAX_TEXT_INPUT);
    let output = extract_from_svg(&text).expect("deep but valid SVG");
    assert_eq!(output.shape.aspect_ratio, Some(1.0));
}

#[test]
fn huge_under_limit_gradients_stay_bounded() {
    let declaration = "background: linear-gradient(90deg, #112233 0%, #445566 100%);";
    let mut css = String::from(".a{");
    while css.len() + declaration.len() + 1 <= MAX_TEXT_INPUT {
        css.push_str(declaration);
    }
    css.push('}');
    let output = extract_from_css(&css).expect("bounded CSS");
    assert_eq!(
        output.colors,
        vec!["#112233".to_string(), "#445566".to_string()]
    );
    assert_eq!(output.gradients, vec!["linear-gradient".to_string()]);
}

#[test]
fn unbalanced_css_is_a_typed_error_not_a_hang() {
    for text in [
        ".a{",
        ".a{color:red}}",
        ".a{content:'unterminated}",
        ".a{/*",
    ] {
        let _ = assert_no_panic(0, "css hostile case", || extract_from_css(text));
        assert!(
            extract_from_css(text).is_err(),
            "expected typed error for {text:?}"
        );
    }
}
