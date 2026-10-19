//! The hedgehog mascot: a one-colour pixel sprite with moods. It's drawn as
//! SVG for web pages (in the text colour) and with half-block characters for
//! terminals (in the terminal's text colour), from one grid.

/// How the hedgehog feels about things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    /// Everything's running.
    Happy,
    /// Nothing to do (no apps yet, or the app isn't running).
    Sleepy,
    /// Something failed and needs a look.
    Worried,
}

impl Mood {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Happy => "All good",
            Self::Sleepy => "Nothing to do yet",
            Self::Worried => "Something needs a look",
        }
    }
}

/// A hedgehog facing right, with headroom above for the mood marks.
/// `X` is drawn; the eye is a hole.
const BASE: [&str; 13] = [
    ".....................",
    ".....................",
    ".....................",
    "......X..X..X........",
    "....X.XX.XX.XX.......",
    "..X.XXXXXXXXXXX......",
    ".XXXXXXXXXXXXXXX.....",
    "XXXXXXXXXXXXXXXXX....",
    ".XXXXXXXXXXXX.XXXX...",
    "XXXXXXXXXXXXX.XXXXXX.",
    ".XXXXXXXXXXXXXXXXXXXX",
    "..XXXXXXXXXXXXXXXX...",
    "...XX.XX....XX.XX....",
];
pub const WIDTH: usize = 21;
pub const HEIGHT: usize = 13;

/// The upper half of the eye; filling it closes the eye (a blink, or sleep).
const LID: (usize, usize) = (13, 8);
/// A small "z".
const SNOOZE: [(usize, usize); 10] = [
    (15, 0),
    (16, 0),
    (17, 0),
    (18, 0),
    (17, 1),
    (16, 2),
    (15, 3),
    (16, 3),
    (17, 3),
    (18, 3),
];
/// An "!".
const BANG: [(usize, usize); 4] = [(16, 0), (16, 1), (16, 2), (16, 4)];

/// The sprite's pixels for a mood, row by row; `true` is drawn.
pub fn grid(mood: Mood) -> Vec<Vec<bool>> {
    let mut grid: Vec<Vec<bool>> = BASE
        .iter()
        .map(|row| row.chars().map(|c| c == 'X').collect())
        .collect();
    let mut fill = |points: &[(usize, usize)]| {
        for &(x, y) in points {
            grid[y][x] = true;
        }
    };
    match mood {
        Mood::Happy => {}
        Mood::Sleepy => {
            fill(&[LID]);
            fill(&SNOOZE);
        }
        Mood::Worried => fill(&BANG),
    }
    grid
}

/// An SVG of the sprite in `currentColor`, `pixel` CSS pixels per sprite
/// pixel. The eyelid is a hidden rect with `class="lid"`, so a page can make
/// the hedgehog blink by showing it for a moment.
pub fn svg(mood: Mood, pixel: u32) -> String {
    let mut rects = String::new();
    for (y, row) in grid(mood).iter().enumerate() {
        let mut x = 0;
        while x < row.len() {
            if !row[x] {
                x += 1;
                continue;
            }
            // One rect per run of drawn pixels.
            let start = x;
            while x < row.len() && row[x] {
                x += 1;
            }
            rects.push_str(&format!(
                "<rect x=\"{start}\" y=\"{y}\" width=\"{}\" height=\"1\"/>",
                x - start
            ));
        }
    }
    let (lid_x, lid_y) = LID;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {WIDTH} {HEIGHT}\" \
         width=\"{}\" height=\"{}\" shape-rendering=\"crispEdges\" role=\"img\" \
         aria-label=\"hedgehog: {}\"><g fill=\"currentColor\">{rects}\
         <rect class=\"lid\" x=\"{lid_x}\" y=\"{lid_y}\" width=\"1\" height=\"1\" opacity=\"0\"/>\
         </g></svg>",
        WIDTH as u32 * pixel,
        HEIGHT as u32 * pixel,
        mood.describe().to_lowercase(),
    )
}

/// Black on light backgrounds, white on dark ones, for SVGs shown on their
/// own (favicons, the README) where there's no page text colour to follow.
const SCHEME: &str = "svg{color:#000}@media (prefers-color-scheme:dark){svg{color:#fff}}";

/// A standalone SVG file of the sprite that blinks on its own, for places
/// like the README where the page can't add its own styles.
pub fn standalone_svg(mood: Mood, pixel: u32) -> String {
    svg(mood, pixel).replacen(
        "<g ",
        &format!(
            "<style>{SCHEME}.lid{{animation:blink 5s infinite}}\
             @keyframes blink{{0%,92%,100%{{opacity:0}}93%,96%{{opacity:1}}}}</style><g "
        ),
        1,
    )
}

/// The happy sprite as a favicon `data:` URL, cropped to the hedgehog.
pub fn favicon() -> String {
    let svg = svg(Mood::Happy, 1)
        .replace(
            &format!("viewBox=\"0 0 {WIDTH} {HEIGHT}\""),
            "viewBox=\"0 -2.5 21 15.5\"",
        )
        .replacen("<g ", &format!("<style>{SCHEME}</style><g "), 1);
    let encoded: String = svg
        .chars()
        .map(|c| match c {
            '%' => "%25".to_string(),
            '<' => "%3C".to_string(),
            '>' => "%3E".to_string(),
            '#' => "%23".to_string(),
            '"' => "'".to_string(),
            c => c.to_string(),
        })
        .collect();
    format!("data:image/svg+xml,{encoded}")
}

/// The sprite as terminal lines, each character cell two pixels stacked
/// (`▀`, `▄`, `█`), in the terminal's own text colour.
pub fn terminal(mood: Mood) -> Vec<String> {
    let grid = grid(mood);
    grid.chunks(2)
        .map(|pair| {
            (0..WIDTH)
                .map(|x| {
                    let top = pair[0][x];
                    let bottom = pair.get(1).is_some_and(|row| row[x]);
                    match (top, bottom) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    }
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_is_rectangular_and_moods_differ() {
        for row in BASE {
            assert_eq!(row.len(), WIDTH);
        }
        assert_eq!(BASE.len(), HEIGHT);
        let happy = grid(Mood::Happy);
        let sleepy = grid(Mood::Sleepy);
        let worried = grid(Mood::Worried);
        assert!(!happy[LID.1][LID.0], "the eye is open");
        assert!(sleepy[LID.1][LID.0], "sleepy eyes are closed");
        assert!(worried[0][16]);
        assert_ne!(happy, worried);
    }

    #[test]
    fn svg_is_well_formed_and_sized() {
        let svg = svg(Mood::Happy, 3);
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(svg.contains("width=\"63\" height=\"39\""));
        assert!(svg.contains("fill=\"currentColor\""));
        assert!(svg.contains("class=\"lid\""));
        assert_eq!(svg.matches("<svg").count(), 1);
    }

    /// `assets/hedgehog.svg` (shown in the README) is generated from the grid.
    /// Regenerate it with `UPDATE_SPRITE=1 cargo test -p hedgehog-core sprite`.
    #[test]
    fn readme_image_matches_the_sprite() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/hedgehog.svg");
        let expected = standalone_svg(Mood::Happy, 6) + "\n";
        if std::env::var_os("UPDATE_SPRITE").is_some() {
            std::fs::write(path, &expected).unwrap();
        }
        let actual = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            actual == expected,
            "assets/hedgehog.svg is out of date; run UPDATE_SPRITE=1 cargo test -p hedgehog-core sprite"
        );
    }

    #[test]
    fn favicon_is_a_data_url() {
        let icon = favicon();
        assert!(icon.starts_with("data:image/svg+xml,%3Csvg"));
        assert!(!icon.contains('"') && !icon.contains('#') && !icon.contains('<'));
        assert!(icon.contains("prefers-color-scheme"));
    }

    #[test]
    fn terminal_rows_pair_up_pixels() {
        let lines = terminal(Mood::Happy);
        assert_eq!(lines.len(), HEIGHT.div_ceil(2));
        for line in &lines {
            assert_eq!(line.chars().count(), WIDTH);
        }
        assert!(lines[5].contains('█'));
    }
}
