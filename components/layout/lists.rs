/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use style::counter_style::{CounterStyle, Symbol, SymbolsType};
use style::properties::longhands::list_style_type::computed_value::T as ListStyleType;
use style::values::computed::Image;
use style::values::generics::counters::Content;
use stylo_atoms::atom;

use crate::context::LayoutContext;
use crate::counters::LIST_ITEM;
use crate::dom_traversal::{
    NodeAndStyleInfo, PseudoElementContentItem, generate_pseudo_element_content,
};
use crate::replaced::ReplacedContents;

/// <https://drafts.csswg.org/css-lists/#content-property>
pub(crate) fn make_marker<'dom>(
    context: &LayoutContext,
    info: &NodeAndStyleInfo<'dom>,
) -> Option<(NodeAndStyleInfo<'dom>, Vec<PseudoElementContentItem>)> {
    let marker_info =
        info.with_pseudo_element(context, style::selector_parser::PseudoElement::Marker)?;
    let style = &marker_info.style;
    let list_style = style.get_list();

    // https://drafts.csswg.org/css-lists/#marker-image
    let marker_image = || match &list_style.list_style_image {
        Image::Url(url) => Some(vec![
            PseudoElementContentItem::Replaced(ReplacedContents::from_image_url(
                marker_info.node,
                context,
                url,
            )?),
            PseudoElementContentItem::Text(" ".into()),
        ]),
        // XXX: Non-None image types unimplemented.
        Image::ImageSet(..) |
        Image::Gradient(..) |
        Image::Image(..) |
        Image::CrossFade(..) |
        Image::PaintWorklet(..) |
        Image::None => None,
        Image::LightDark(..) => unreachable!("light-dark() should be disabled"),
    };

    let content = match &marker_info.style.get_counters().content {
        Content::Items(_) => generate_pseudo_element_content(&marker_info, context),
        Content::None => return None,
        Content::Normal => marker_image().or_else(|| {
            let value = context
                .counter_values(&marker_info.node, &LIST_ITEM)
                .last()
                .copied()
                .unwrap_or_default();
            Some(vec![PseudoElementContentItem::Text(marker_string(
                &list_style.list_style_type,
                value,
            )?)])
        })?,
    };

    Some((marker_info, content))
}

fn symbol_to_string(symbol: &Symbol) -> &str {
    match symbol {
        Symbol::String(string) => string,
        Symbol::Ident(ident) => &ident.0,
    }
}

/// <https://drafts.csswg.org/css-counter-styles-3/#generate-a-counter>
pub(crate) fn generate_counter_representation(counter_style: &CounterStyle, value: i32) -> String {
    match counter_style {
        CounterStyle::None | CounterStyle::String(_) => unreachable!("Invalid counter style"),
        CounterStyle::Name(name) => match name.0 {
            atom!("disc") => "\u{2022}".into(),            /* "•" */
            atom!("circle") => "\u{25E6}".into(),          /* "◦" */
            atom!("square") => "\u{25AA}".into(),          /* "▪" */
            atom!("disclosure-open") => "\u{25BE}".into(), /* "▾" */
            // TODO: Use U+25C2 "◂" depending on the direction.
            atom!("disclosure-closed") => "\u{25B8}".into(), /* "▸" */
            atom!("decimal-leading-zero") => {
                let decimal = decimal(value.unsigned_abs());
                let padded = if decimal.len() < 2 {
                    format!("0{decimal}")
                } else {
                    decimal
                };
                with_sign(value, padded)
            },
            atom!("arabic-indic") => digits_from_zero(value, '\u{660}'), /* "٠" */
            atom!("bengali") => digits_from_zero(value, '\u{9E6}'),      /* "০" */
            atom!("cambodian") | atom!("khmer") => digits_from_zero(value, '\u{17E0}'), /* "០" */
            atom!("devanagari") => digits_from_zero(value, '\u{966}'),   /* "०" */
            atom!("gujarati") => digits_from_zero(value, '\u{AE6}'),     /* "૦" */
            atom!("gurmukhi") => digits_from_zero(value, '\u{A66}'),     /* "੦" */
            atom!("kannada") => digits_from_zero(value, '\u{CE6}'),      /* "೦" */
            atom!("lao") => digits_from_zero(value, '\u{ED0}'),          /* "໐" */
            atom!("malayalam") => digits_from_zero(value, '\u{D66}'),    /* "൦" */
            atom!("mongolian") => digits_from_zero(value, '\u{1810}'),   /* "᠐" */
            atom!("myanmar") => digits_from_zero(value, '\u{1040}'),     /* "၀" */
            atom!("oriya") => digits_from_zero(value, '\u{B66}'),        /* "୦" */
            atom!("persian") => digits_from_zero(value, '\u{6F0}'),      /* "۰" */
            atom!("tamil") => digits_from_zero(value, '\u{BE6}'),        /* "௦" */
            atom!("telugu") => digits_from_zero(value, '\u{C66}'),       /* "౦" */
            atom!("thai") => digits_from_zero(value, '\u{E50}'),         /* "๐" */
            atom!("tibetan") => digits_from_zero(value, '\u{F20}'),      /* "༠" */
            atom!("cjk-decimal") => numeric(
                value,
                &["〇", "一", "二", "三", "四", "五", "六", "七", "八", "九"],
            ),
            atom!("lower-roman") => roman(value, false),
            atom!("upper-roman") => roman(value, true),
            atom!("lower-alpha") | atom!("lower-latin") => {
                alphabetic(value, &LATIN_LETTERS, str::to_string)
            },
            atom!("upper-alpha") | atom!("upper-latin") => {
                alphabetic(value, &LATIN_LETTERS, str::to_ascii_uppercase)
            },
            atom!("lower-greek") => alphabetic(value, &GREEK_LETTERS, str::to_string),
            // TODO: The CJK, Korean, Ethiopic, Hebrew, Armenian and Georgian systems fall
            // back to decimal.
            _ => with_sign(value, decimal(value.unsigned_abs())),
        },
        CounterStyle::Symbols { ty, symbols } => {
            let symbols: Vec<&str> = symbols.0.iter().map(symbol_to_string).collect();
            let count = symbols.len() as i64;
            let value = value as i64;
            match ty {
                SymbolsType::Cyclic => symbols[(value - 1).rem_euclid(count) as usize].into(),
                SymbolsType::Numeric => numeric(value as i32, &symbols),
                SymbolsType::Alphabetic => alphabetic(value as i32, &symbols, str::to_string),
                SymbolsType::Symbolic if value >= 1 => {
                    let symbol = symbols[((value - 1) % count) as usize];
                    symbol.repeat(((value - 1) / count + 1) as usize)
                },
                SymbolsType::Fixed if (1..=count).contains(&value) => {
                    symbols[(value - 1) as usize].into()
                },
                // Out of range: fall back to `decimal`.
                SymbolsType::Symbolic | SymbolsType::Fixed => {
                    with_sign(value as i32, decimal(value.unsigned_abs() as u32))
                },
            }
        },
    }
}

const LATIN_LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
    "s", "t", "u", "v", "w", "x", "y", "z",
];

const GREEK_LETTERS: [&str; 24] = [
    "α", "β", "γ", "δ", "ε", "ζ", "η", "θ", "ι", "κ", "λ", "μ", "ν", "ξ", "ο", "π", "ρ", "σ",
    "τ", "υ", "φ", "χ", "ψ", "ω",
];

fn decimal(value: u32) -> String {
    value.to_string()
}

/// A negative value of a style with an unbounded range gets a `-` in front.
fn with_sign(value: i32, representation: String) -> String {
    if value < 0 {
        format!("-{representation}")
    } else {
        representation
    }
}

/// <https://drafts.csswg.org/css-counter-styles-3/#numeric-system>
fn numeric(value: i32, symbols: &[&str]) -> String {
    let base = symbols.len() as u32;
    let mut remaining = value.unsigned_abs();
    let mut digits = vec![symbols[(remaining % base) as usize]];
    remaining /= base;
    while remaining > 0 {
        digits.push(symbols[(remaining % base) as usize]);
        remaining /= base;
    }
    digits.reverse();
    with_sign(value, digits.concat())
}

/// A numeric style whose ten digits follow each other in Unicode.
fn digits_from_zero(value: i32, zero: char) -> String {
    let digits = decimal(value.unsigned_abs())
        .chars()
        .filter_map(|digit| char::from_u32(zero as u32 + digit.to_digit(10)?))
        .collect();
    with_sign(value, digits)
}

/// <https://drafts.csswg.org/css-counter-styles-3/#alphabetic-system>. Defined from 1
/// up; anything else falls back to `decimal`.
fn alphabetic(value: i32, symbols: &[&str], transform: fn(&str) -> String) -> String {
    if value < 1 {
        return with_sign(value, decimal(value.unsigned_abs()));
    }
    let base = symbols.len() as u32;
    let mut remaining = value as u32;
    let mut letters = Vec::new();
    while remaining > 0 {
        remaining -= 1;
        letters.push(symbols[(remaining % base) as usize]);
        remaining /= base;
    }
    letters.reverse();
    transform(&letters.concat())
}

/// The additive `lower-roman` and `upper-roman` styles, defined from 1 to 3999.
fn roman(value: i32, upper: bool) -> String {
    const NUMERALS: [(i32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    if !(1..=3999).contains(&value) {
        return with_sign(value, decimal(value.unsigned_abs()));
    }
    let mut remaining = value;
    let mut representation = String::new();
    for (weight, numeral) in NUMERALS {
        while remaining >= weight {
            representation.push_str(numeral);
            remaining -= weight;
        }
    }
    if upper {
        representation.make_ascii_uppercase();
    }
    representation
}

/// <https://drafts.csswg.org/css-lists/#marker-string>
pub(crate) fn marker_string(list_style_type: &ListStyleType, value: i32) -> Option<String> {
    let suffix = match &list_style_type.0 {
        CounterStyle::None => return None,
        CounterStyle::String(string) => return Some(string.to_string()),
        CounterStyle::Name(name) => match name.0 {
            atom!("disc") |
            atom!("circle") |
            atom!("square") |
            atom!("disclosure-open") |
            atom!("disclosure-closed") => " ",
            atom!("hiragana") |
            atom!("hiragana-iroha") |
            atom!("katakana") |
            atom!("katakana-iroha") |
            atom!("cjk-decimal") |
            atom!("cjk-earthly-branch") |
            atom!("cjk-heavenly-stem") |
            atom!("japanese-informal") |
            atom!("japanese-formal") |
            atom!("simp-chinese-informal") |
            atom!("simp-chinese-formal") |
            atom!("trad-chinese-informal") |
            atom!("trad-chinese-formal") |
            atom!("cjk-ideographic") => "\u{3001}", /* "、" */
            atom!("korean-hangul-formal") |
            atom!("korean-hanja-informal") |
            atom!("korean-hanja-formal") => ", ",
            atom!("ethiopic-numeric") => "/ ",
            _ => ". ",
        },
        CounterStyle::Symbols { .. } => " ",
    };
    Some(format!(
        "{}{}",
        generate_counter_representation(&list_style_type.0, value),
        suffix
    ))
}
