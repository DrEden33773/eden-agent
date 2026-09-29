//! Grok Build 2bdd1d6 line_utils.rs, Apache-2.0, copyright 2023–2026 xAI.
//! Local changes: crate-private visibility; used by Eden status/composer/footer surfaces.
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
pub(crate) fn fit_line_to_width<'a>(line: Line<'a>, width: usize) -> Line<'a> {
    let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if total == width {
        return line;
    }

    let Line {
        style,
        alignment,
        mut spans,
    } = line;

    if total < width {
        spans.push(Span::raw(" ".repeat(width - total)));
        return Line {
            style,
            alignment,
            spans,
        };
    }

    // Wider than width: clip on grapheme boundaries, no ellipsis.
    let mut out: Vec<Span<'a>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let sw = span.content.width();
        if used + sw <= width {
            used += sw;
            out.push(span);
            if used == width {
                break;
            }
            continue;
        }
        // This span straddles the boundary: take whole graphemes that fit
        let remaining = width - used;
        let mut taken = String::new();
        let mut taken_width = 0usize;
        for g in span.content.graphemes(true) {
            let gw = g.width();
            if taken_width + gw > remaining {
                break;
            }
            taken_width += gw;
            taken.push_str(g);
        }
        if !taken.is_empty() {
            out.push(Span::styled(taken, span.style));
            used += taken_width;
        }
        // A straddling wide grapheme leaves a 1-column gap; pad it.
        if used < width {
            out.push(Span::raw(" ".repeat(width - used)));
        }
        break;
    }

    Line {
        style,
        alignment,
        spans: out,
    }
}
