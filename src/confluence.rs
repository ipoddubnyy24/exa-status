//! Reads the weekly PM status pages (Confluence space PR). Each page holds a
//! two-column table: Date of Update, Status, Scope, Target Date, Risks, ...
//! The storage format is XHTML, walked with an XML reader so nested tables,
//! entities and struck-through text are handled as a browser shows them.

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate};
use quick_xml::XmlVersion;
use quick_xml::escape::{resolve_html5_entity, resolve_xml_entity};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use regex::{Captures, Regex};
use serde::Deserialize;
use std::borrow::Cow;
use std::ops::Range;
use url::Url;

use crate::jira::Jira;
use crate::model::{Instance, PmRef};

/// Row labels read from the status table, compared without case or a trailing colon.
const DATE_OF_UPDATE: &str = "date of update";
const STATUS: &str = "status";
const TARGET_DATE: &str = "target date";

/// A date in the Target Date cell, with the label written in front of it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LabelledDate {
    /// "Beta" for "Beta: 30 Nov 2026"; `None` when unlabelled.
    pub(crate) label: Option<String>,
    /// The date.
    pub(crate) date: NaiveDate,
}

/// What one PM status page says.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PmPage {
    /// The page as the UI shows it.
    pub(crate) info: PmRef,
    /// Dates found in the Target Date cell, in page order.
    pub(crate) dates: Vec<LabelledDate>,
}

impl PmPage {
    /// The date labelled `label`, or the first date when `label` is `None`.
    pub(crate) fn target(&self, label: Option<&str>) -> Option<NaiveDate> {
        let found = match label {
            Some(l) => self.dates.iter().find(|d| {
                d.label
                    .as_deref()
                    .is_some_and(|x| x.eq_ignore_ascii_case(l))
            }),
            None => self.dates.first(),
        };
        found.map(|d| d.date)
    }
}

/// The page reply fields the dashboard needs; anything missing is an error.
#[derive(Deserialize)]
struct PageReply {
    title: String,
    body: Body,
    #[serde(rename = "_links")]
    links: Option<Links>,
}

#[derive(Deserialize)]
struct Body {
    storage: Storage,
}

#[derive(Deserialize)]
struct Storage {
    value: String,
}

#[derive(Deserialize)]
struct Links {
    webui: Option<String>,
}

/// Date patterns, compiled once.
pub(crate) struct PmParser {
    iso: Regex,
    day_month_year: Regex,
    month_day_year: Regex,
    month_year: Regex,
    label: Regex,
}

/// One row of a top-level table: its cells, each as lines of visible text.
type Row = Vec<Vec<String>>;

/// A date found in a line, by byte range.
struct Found {
    span: Range<usize>,
    /// `None` when the text looks like a date but is not one ("31 Feb 2027").
    date: Option<NaiveDate>,
}

impl PmParser {
    /// Compiles the patterns.
    pub(crate) fn new() -> Result<Self> {
        let re =
            |pattern: &str| Regex::new(pattern).with_context(|| format!("compiling {pattern:?}"));
        Ok(Self {
            iso: re(r"\b(\d{4})-(\d{2})-(\d{2})\b")?,
            day_month_year: re(r"\b(\d{1,2})(?:st|nd|rd|th)? ([A-Za-z]+)\.?,? (\d{4})\b")?,
            month_day_year: re(r"\b([A-Za-z]+)\.? (\d{1,2})(?:st|nd|rd|th)?,? (\d{4})\b")?,
            month_year: re(r"\b([A-Za-z]+)\.? (\d{4})\b")?,
            label: re(r"^[A-Za-z][A-Za-z0-9 .]{0,20}$")?,
        })
    }

    /// Reads the status table out of a page's storage markup. Where a label
    /// appears twice, the first row wins (the status table opens the page).
    pub(crate) fn parse(&self, title: &str, url: Url, storage: &str) -> Result<PmPage> {
        let mut page = PmPage {
            info: PmRef {
                title: title.into(),
                url,
                status: None,
                target: None,
                updated: None,
            },
            dates: Vec::new(),
        };
        let (mut seen_update, mut seen_status, mut seen_target) = (false, false, false);
        for row in rows(storage)? {
            let (Some(label), Some(value)) = (row.first(), row.get(1)) else {
                continue;
            };
            let label = label.join(" ");
            match label
                .trim()
                .trim_end_matches(':')
                .trim()
                .to_lowercase()
                .as_str()
            {
                DATE_OF_UPDATE if !seen_update => {
                    seen_update = true;
                    page.info.updated = value.iter().find_map(|l| self.first_date(l));
                }
                STATUS if !seen_status => {
                    seen_status = true;
                    page.info.status = value.first().cloned();
                }
                TARGET_DATE if !seen_target => {
                    seen_target = true;
                    page.info.target = Some(value.join(" ")).filter(|t| !t.is_empty());
                    page.dates = value
                        .iter()
                        .flat_map(|line| self.labelled_dates(line))
                        .collect();
                }
                _ => {}
            }
        }
        Ok(page)
    }

    /// The earliest date in a line.
    pub(crate) fn first_date(&self, line: &str) -> Option<NaiveDate> {
        self.dates_in(line).into_iter().find_map(|f| f.date)
    }

    /// Every date in a line with the label written just before it: "Beta",
    /// for "Beta: 30 Nov 2026", "GA - 8 Mar 2027" or "..., GA 8 Mar 2027".
    pub(crate) fn labelled_dates(&self, line: &str) -> Vec<LabelledDate> {
        let mut out = Vec::new();
        let mut from = 0;
        for found in self.dates_in(line) {
            let before = line.get(from..found.span.start).unwrap_or_default();
            from = found.span.end;
            let Some(date) = found.date else { continue };
            let text = before.trim_start_matches(|c: char| c.is_whitespace() || ",;".contains(c));
            let text = text
                .trim_end_matches(|c: char| c.is_whitespace() || ":-\u{2013}\u{2014}".contains(c));
            let label = self.label.is_match(text).then(|| text.to_string());
            out.push(LabelledDate { label, date });
        }
        out
    }

    /// The dates in a line, in order: ISO, "18 Jan 2027", "8th March 2027",
    /// "Nov 30, 2026", or "Nov 2027", which means the last day of that month.
    /// Where patterns overlap the earliest, longest match counts, so an
    /// impossible "31 Feb 2027" is not read as "Feb 2027".
    fn dates_in(&self, line: &str) -> Vec<Found> {
        let mut candidates: Vec<Found> = Vec::new();
        let span = |c: &Captures<'_>| c.get(0).map(|m| m.range());
        let num =
            |c: &Captures<'_>, i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
        let word = |c: &Captures<'_>, i: usize| c.get(i).and_then(|m| month(m.as_str()));
        let year =
            |c: &Captures<'_>, i: usize| c.get(i).and_then(|m| m.as_str().parse::<i32>().ok());
        for c in self.iso.captures_iter(line) {
            let date = NaiveDate::from_ymd_opt(
                year(&c, 1).unwrap_or(0),
                num(&c, 2).unwrap_or(0),
                num(&c, 3).unwrap_or(0),
            );
            candidates.extend(span(&c).map(|span| Found { span, date }));
        }
        for c in self.day_month_year.captures_iter(line) {
            let date =
                word(&c, 2).and_then(|m| NaiveDate::from_ymd_opt(year(&c, 3)?, m, num(&c, 1)?));
            candidates.extend(span(&c).map(|span| Found { span, date }));
        }
        for c in self.month_day_year.captures_iter(line) {
            // Only when the first word is a month: "Release 30, 2026" is not a date.
            let Some(m) = word(&c, 1) else { continue };
            let date =
                NaiveDate::from_ymd_opt(year(&c, 3).unwrap_or(0), m, num(&c, 2).unwrap_or(0));
            candidates.extend(span(&c).map(|span| Found { span, date }));
        }
        for c in self.month_year.captures_iter(line) {
            let Some(m) = word(&c, 1) else { continue };
            let date =
                NaiveDate::from_ymd_opt(year(&c, 2).unwrap_or(0), m, 1).and_then(last_of_month);
            candidates.extend(span(&c).map(|span| Found { span, date }));
        }
        candidates.sort_by_key(|f| (f.span.start, std::cmp::Reverse(f.span.end)));
        let mut kept: Vec<Found> = Vec::new();
        for f in candidates {
            if kept.last().is_none_or(|k| f.span.start >= k.span.end) {
                kept.push(f);
            }
        }
        kept
    }
}

/// The month a word names: a full name or a three-letter abbreviation, any
/// case, and "Sept".
fn month(word: &str) -> Option<u32> {
    let word = if word.eq_ignore_ascii_case("sept") {
        "sep"
    } else {
        word
    };
    NaiveDate::parse_from_str(&format!("1 {word} 2000"), "%d %B %Y")
        .ok()
        .map(|d| d.month())
}

fn last_of_month(first: NaiveDate) -> Option<NaiveDate> {
    let next = if first.month() == 12 {
        NaiveDate::from_ymd_opt(first.year() + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(first.year(), first.month() + 1, 1)?
    };
    next.pred_opt()
}

/// What an open element inside a table cell does to the text under it.
#[derive(Clone, Copy, PartialEq)]
enum Open {
    /// Shown as is.
    Plain,
    /// Struck through, or a macro parameter: not part of the visible text.
    Hidden,
    /// A paragraph or list item: ends a line when it closes.
    Line,
    /// A nested table.
    Table,
}

/// The rows of the page's top-level tables. Text in nested tables stays in
/// the cell that holds them; struck-through text and macro parameters other
/// than a status title are left out; `<time>` gives its date.
fn rows(storage: &str) -> Result<Vec<Row>> {
    let mut reader = Reader::from_str(storage);
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut row: Option<Row> = None;
    let mut cell: Option<Vec<String>> = None;
    let mut line = String::new();
    let mut open: Vec<Open> = Vec::new();
    let mut status_macro = 0usize;
    let hidden = |open: &[Open]| open.contains(&Open::Hidden);
    loop {
        match reader.read_event().context("reading the page markup")? {
            Event::Eof => break,
            Event::Start(e) => {
                let name = e.name();
                match name.as_ref() {
                    "table" if cell.is_none() => depth += 1,
                    "tr" if depth == 1 && cell.is_none() => row = Some(Vec::new()),
                    "th" | "td" if depth == 1 && cell.is_none() && row.is_some() => {
                        cell = Some(Vec::new())
                    }
                    _ if cell.is_some() => {
                        let kind = match name.as_ref() {
                            "table" => Open::Table,
                            "p" | "li" | "tr" => Open::Line,
                            "s" | "del" | "strike" => Open::Hidden,
                            "span" if struck(&e) => Open::Hidden,
                            "ac:structured-macro" => {
                                if attribute(&e, "ac:name").as_deref() == Some("status") {
                                    status_macro += 1;
                                }
                                Open::Plain
                            }
                            "ac:parameter" => {
                                let title = attribute(&e, "ac:name").as_deref() == Some("title");
                                if status_macro > 0 && title {
                                    Open::Plain
                                } else {
                                    Open::Hidden
                                }
                            }
                            _ => Open::Plain,
                        };
                        open.push(kind);
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.name().as_ref() {
                "th" | "td" if open.is_empty() && depth == 1 => {
                    if let (Some(mut lines), Some(r)) = (cell.take(), row.as_mut()) {
                        end_line(&mut line, &mut lines);
                        r.push(lines);
                    }
                }
                "tr" if open.is_empty() && depth == 1 && cell.is_none() => out.extend(row.take()),
                "table" if open.is_empty() && cell.is_none() => depth = depth.saturating_sub(1),
                name => {
                    if cell.is_some() {
                        if name == "ac:structured-macro" {
                            status_macro = status_macro.saturating_sub(1);
                        }
                        // Cells of a nested table are separate words, not one run of text.
                        if name == "th" || name == "td" {
                            line.push(' ');
                        }
                        if matches!(open.pop(), Some(Open::Line | Open::Table))
                            && let Some(lines) = cell.as_mut()
                        {
                            end_line(&mut line, lines);
                        }
                    }
                }
            },
            Event::Empty(e) => {
                if let Some(lines) = cell.as_mut().filter(|_| !hidden(&open)) {
                    match e.name().as_ref() {
                        "br" => end_line(&mut line, lines),
                        "time" => {
                            if let Some(date) = attribute(&e, "datetime") {
                                line.push(' ');
                                line.push_str(&date);
                                line.push(' ');
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Text(t) if cell.is_some() && !hidden(&open) => line.push_str(&t.html_content()),
            Event::CData(t) if cell.is_some() && !hidden(&open) => line.push_str(&t.html_content()),
            Event::GeneralRef(r) if cell.is_some() && !hidden(&open) => {
                let name = r.html_content();
                match r.resolve_char_ref() {
                    Ok(Some(c)) => line.push(c),
                    _ => match resolve_xml_entity(&name).or_else(|| resolve_html5_entity(&name)) {
                        Some(text) => line.push_str(text),
                        None => {
                            line.push('&');
                            line.push_str(&name);
                            line.push(';');
                        }
                    },
                }
            }
            _ => {}
        }
    }
    if depth != 0 || cell.is_some() {
        bail!("the page markup ends inside a table");
    }
    Ok(out)
}

/// Closes the current line of a cell, keeping it when it has visible text.
fn end_line(line: &mut String, lines: &mut Vec<String>) {
    let text = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if !text.is_empty() {
        lines.push(text);
    }
    line.clear();
}

/// An attribute's value, if present and readable.
fn attribute(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.try_get_attribute(name)
        .ok()
        .flatten()?
        .normalized_value(XmlVersion::default())
        .ok()
        .map(Cow::into_owned)
}

/// A span styled as struck through.
fn struck(e: &BytesStart<'_>) -> bool {
    attribute(e, "style").is_some_and(|s| {
        s.replace(' ', "")
            .to_lowercase()
            .contains("text-decoration:line-through")
    })
}

/// Reads one PM status page through the Jira cloud credentials (same Atlassian site).
pub(crate) fn fetch(jira: &Jira, parser: &PmParser, id: u64) -> Result<PmPage> {
    let reply = jira
        .get(
            Instance::Cloud,
            &format!("wiki/api/v2/pages/{id}"),
            &[("body-format", "storage")],
        )
        .with_context(|| format!("reading Confluence page {id}"))?;
    let reply =
        PageReply::deserialize(&reply).with_context(|| format!("decoding Confluence page {id}"))?;
    let link = match reply.links.and_then(|l| l.webui) {
        Some(webui) => format!("wiki/{}", webui.trim_start_matches('/')),
        None => format!("wiki/pages/viewpage.action?pageId={id}"),
    };
    let url = jira
        .base(Instance::Cloud)
        .join(&link)
        .with_context(|| format!("building the link to page {id}"))?;
    parser
        .parse(&reply.title, url, &reply.body.storage.value)
        .with_context(|| format!("reading Confluence page {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const EXA7: &str = r#"<table><tbody>
<tr><th><p><strong><u>Date of Update</u></strong></p></th><td><p><time datetime="2026-10-13" /></p></td></tr>
<tr><th><p><strong><u>Status</u></strong></p></th><td><p><ac:structured-macro ac:name="status" ac:schema-version="1"><ac:parameter ac:name="title">ON TRACK</ac:parameter><ac:parameter ac:name="colour">Green</ac:parameter></ac:structured-macro>  </p></td></tr>
<tr><th><p><strong><u>Target Date</u></strong></p></th><td><p><span style="color: rgb(7,71,166);">Beta: </span><time datetime="2026-11-30" /><span> </span></p><p><span>GA: </span><time datetime="2027-03-08" /></p></td></tr>
<tr><th><p>Risks</p></th><td><p>IPv6 &amp; KMIP</p></td></tr>
<tr><td><p>row without a header</p></td></tr>
</tbody></table>"#;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn page(storage: &str) -> PmPage {
        PmParser::new()
            .unwrap()
            .parse(
                "EXAScaler 7",
                Url::parse("https://x/wiki/p").unwrap(),
                storage,
            )
            .unwrap()
    }

    #[test]
    fn reads_status_update_and_labelled_dates() {
        let p = page(EXA7);

        assert_eq!(p.info.status.as_deref(), Some("ON TRACK"));
        assert_eq!(p.info.updated, Some(d(2026, 10, 13)));
        assert_eq!(p.target(Some("beta")), Some(d(2026, 11, 30)));
        assert_eq!(p.target(Some("GA")), Some(d(2027, 3, 8)));
        assert_eq!(p.target(None), Some(d(2026, 11, 30)));
        assert_eq!(p.target(Some("RC")), None);
        assert_eq!(
            p.info.target.as_deref(),
            Some("Beta: 2026-11-30 GA: 2027-03-08")
        );
    }

    #[test]
    fn a_struck_through_date_is_not_the_target() {
        let p = page(
            r#"<table><tr><td><p>target date:</p></td><td><p><s>15 Nov 2026</s> <span style="text-decoration: line-through;">20 <em>Nov</em> 2026</span> 30 November 2026</p></td></tr></table>"#,
        );

        assert_eq!(
            (p.target(None), p.info.target.as_deref()),
            (Some(d(2026, 11, 30)), Some("30 November 2026"))
        );
    }

    #[test]
    fn nested_tables_entities_and_later_duplicates_do_not_confuse_the_rows() {
        let p = page(
            r#"<table><tr><th>Status</th><td><p>AT RISK &ndash; see &#8220;Risks&#8221;</p></td></tr>
<tr><th>Risks</th><td><table><tr><th>Status</th><td>closed</td></tr></table></td></tr>
<tr><th>Target Date</th><td><p>Nov 2027 &nbsp;</p></td></tr></table>
<table><tr><th>Status</th><td>DONE</td></tr></table>"#,
        );

        assert_eq!(
            p.info.status.as_deref(),
            Some("AT RISK \u{2013} see \u{201c}Risks\u{201d}")
        );
        assert_eq!(p.target(None), Some(d(2027, 11, 30)));
    }

    #[test]
    fn a_page_without_the_table_has_no_dates() {
        let p = page("<p>nothing here</p>");

        assert_eq!(
            (p.info.status, p.info.updated, p.info.target, p.dates.len()),
            (None, None, None, 0)
        );
    }

    #[test]
    fn broken_markup_is_an_error() {
        let parsed = PmParser::new().unwrap().parse(
            "x",
            Url::parse("https://x/").unwrap(),
            "<table><tr><td>open",
        );

        assert_eq!(
            parsed.unwrap_err().to_string(),
            "the page markup ends inside a table"
        );
    }

    #[test_case("2027-01-18", Some(d(2027, 1, 18)) ; "iso")]
    #[test_case("18 Jan 2027", Some(d(2027, 1, 18)) ; "day month year")]
    #[test_case("8 March 2027", Some(d(2027, 3, 8)) ; "long month")]
    #[test_case("15th Nov 2026", Some(d(2026, 11, 15)) ; "ordinal day")]
    #[test_case("Nov 30, 2026", Some(d(2026, 11, 30)) ; "month day year")]
    #[test_case("Sept 2027", Some(d(2027, 9, 30)) ; "sept")]
    #[test_case("Nov 2027", Some(d(2027, 11, 30)) ; "month only means last day")]
    #[test_case("Dec 2026", Some(d(2026, 12, 31)) ; "december rolls the year")]
    #[test_case("30 Nov 2026 (was 2026-11-15)", Some(d(2026, 11, 30)) ; "the first date in the line wins")]
    #[test_case("Release 30, 2026", None ; "a word that is not a month")]
    #[test_case("Q1 2027", None ; "quarter")]
    #[test_case("TBD", None ; "no date")]
    #[test_case("31 Feb 2027", None ; "impossible date")]
    fn first_date_reads_the_formats_pages_use(line: &str, expected: Option<NaiveDate>) {
        assert_eq!(PmParser::new().unwrap().first_date(line), expected);
    }

    #[test_case("Beta: 30 Nov 2026, GA: 8 Mar 2027", &[(Some("Beta"), "2026-11-30"), (Some("GA"), "2027-03-08")] ; "two labelled dates")]
    #[test_case("GA - 8 Mar 2027", &[(Some("GA"), "2027-03-08")] ; "dash after the label")]
    #[test_case("30 Nov 2026 (was 2026-11-15)", &[(None, "2026-11-30"), (None, "2026-11-15")] ; "a remark is not a label")]
    #[test_case("Target 2026-11-30", &[(Some("Target"), "2026-11-30")] ; "label without punctuation")]
    fn labelled_dates_read_every_date_in_a_line(line: &str, expected: &[(Option<&str>, &str)]) {
        let found = PmParser::new().unwrap().labelled_dates(line);

        let expected: Vec<LabelledDate> = expected
            .iter()
            .map(|(label, date)| LabelledDate {
                label: label.map(String::from),
                date: date.parse().unwrap(),
            })
            .collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn nested_cells_stay_separate_words() {
        let p = page(
            r#"<table><tr><th>Target Date</th><td><table><tr><td>GA</td><td>8 Mar 2027</td></tr></table></td></tr></table>"#,
        );

        assert_eq!(p.target(Some("GA")), Some(d(2027, 3, 8)));
    }
}
