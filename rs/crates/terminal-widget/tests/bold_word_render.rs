//! End-to-end check of the artifact a user actually reported: inside a single bold word, the
//! `i` rendered grey while `m`, `a` and `n` rendered bright white.
//!
//! This drives the real `SoftwareRenderer` — the only renderer the app ever constructs — over a
//! grid holding a bold "main", using the exact colours measured off the screen at the time
//! (a 214/255 foreground on an 18/255 background at 14px). It then compares the brightest pixel
//! of each letter's cell. Before the fix the `i` cell peaked at 81 against its neighbours' 214.

use hyperpanes_terminal_widget::{
    Font, GridSnapshot, PaneRenderer, RenderCell, RenderOpts, SoftwareRenderer,
};

const FG: [u8; 4] = [214, 214, 214, 255];
const BG: [u8; 4] = [18, 18, 18, 255];

fn font() -> Font {
    let src = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../app/assets/fonts/JetBrainsMono-Regular.ttf"
    );
    Font::from_path(src, 14.0).unwrap()
}

/// Brightest luminance in the column range `[x0, x1)` of `px` (RGBA, `w` wide).
fn peak(px: &[u8], w: u32, x0: u32, x1: u32) -> u32 {
    let mut best = 0;
    for y in 0..(px.len() as u32 / (w * 4)) {
        for x in x0..x1.min(w) {
            let i = ((y * w + x) * 4) as usize;
            let l = (299 * px[i] as u32 + 587 * px[i + 1] as u32 + 114 * px[i + 2] as u32) / 1000;
            best = best.max(l);
        }
    }
    best
}

#[test]
fn every_letter_of_a_bold_word_is_equally_bright() {
    let mut f = font();
    let cw = f.cell_w;
    let word: Vec<char> = "main".chars().collect();
    let cells: Vec<RenderCell> = word
        .iter()
        .map(|&ch| RenderCell {
            ch,
            fg: FG,
            bg: BG,
            bold: true,
            ..Default::default()
        })
        .collect();
    let grid = GridSnapshot {
        cols: word.len(),
        rows: 1,
        cells,
        cursor: (0, 0),
        cursor_visible: false,
        default_bg: BG,
        default_fg: FG,
    };

    let img = SoftwareRenderer::new().render(&grid, &mut f, &RenderOpts { cursor_on: false });
    let buf = img
        .to_rgba8()
        .expect("software renderer produces an rgba8 image");
    let (w, px) = (buf.width(), buf.as_bytes());

    let peaks: Vec<u32> = (0..word.len() as u32)
        .map(|c| peak(px, w, c * cw, (c + 1) * cw))
        .collect();
    let brightest = *peaks.iter().max().unwrap();

    for (ch, &p) in word.iter().zip(&peaks) {
        // The reported bug was a 2.6x gap between `i` and its neighbours. Bold letters of one
        // word, one colour, one size have no business differing by more than rounding.
        assert!(
            p * 100 >= brightest * 90,
            "bold '{ch}' peaks at {p}, well under the word's {brightest} — {peaks:?}"
        );
    }
    // And the word really is drawn at the requested brightness, not merely uniformly dim.
    assert!(
        brightest >= 200,
        "word peaked at {brightest}, expected ~214 — {peaks:?}"
    );
}
