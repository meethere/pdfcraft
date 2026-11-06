//! `pdfcraft-ungroup file.pdf [--open pdfcraft.exe]`: saves `file-ungrouped.pdf`, with the Form
//! artwork of every page drawn in place, so an official PdfCraft that has no Ungroup yet can edit
//! its text. The original is never changed. A personal stopgap on this fork until #3067 ships.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use pdfcraft_automation::{Automation, Content, Pixels};
use serde_json::{Value, json};

/// Forms inside forms surface one level per pass; this bounds a form that draws itself.
const MAX_PASSES: usize = 16;
/// Ungroup attempts on one page, for the same reason: a self-drawing form can double each pass.
const MAX_ATTEMPTS_PER_PAGE: usize = 2_000;
/// Numbered output names tried before giving up.
const MAX_OUTPUT_NAMES: usize = 1_000;
/// Scale of the before and after renders. Low: it's a safety check, not a picture.
const CHECK_SCALE: f32 = 1.0;

#[derive(Debug, Default, PartialEq)]
struct Report {
    /// The file written; `None` when there was nothing to ungroup.
    output: Option<PathBuf>,
    ungrouped: usize,
    /// Page (1-based) and reason, for artwork left as it was.
    refused: Vec<(u64, String)>,
    /// Pages (1-based) whose pixels changed. Ungrouping should never change them.
    changed_look: Vec<u64>,
}

fn result_json(content: Vec<Content>) -> Value {
    content
        .into_iter()
        .find_map(|c| match c {
            Content::Json(v) => Some(v),
            Content::Png { .. } => None,
        })
        .unwrap_or(Value::Null)
}

fn call(auto: &mut Automation, tool: &str, args: &Value) -> Result<Value, String> {
    auto.call(tool, args).map(result_json).map_err(|e| e.to_string())
}

/// The numbers of the page's Form artwork, in drawing order.
fn form_numbers(auto: &mut Automation, doc: u64, page: u64) -> Result<Vec<u64>, String> {
    let list = call(auto, "page_images", &json!({ "doc": doc, "page": page }))?;
    let images = list["images"].as_array().map(Vec::as_slice).unwrap_or_default();
    Ok(images.iter().filter(|i| i["kind"] == "form").filter_map(|i| i["image"].as_u64()).collect())
}

/// Ungroups all it can on one page: how many, and the reasons for what stayed. A form becomes its
/// contents at its own place in the order, so going from the last down keeps the lower numbers
/// true; forms that were inside it are found by the next pass.
fn ungroup_page(auto: &mut Automation, doc: u64, page: u64) -> Result<(usize, Vec<String>), String> {
    let (mut done, mut attempts) = (0usize, 0usize);
    let mut refused = Vec::new();
    for _ in 0..MAX_PASSES {
        refused.clear();
        let before = done;
        for image in form_numbers(auto, doc, page)?.into_iter().rev() {
            attempts = attempts.saturating_add(1);
            if attempts > MAX_ATTEMPTS_PER_PAGE {
                refused.push(format!("stopped after {MAX_ATTEMPTS_PER_PAGE} attempts"));
                return Ok((done, refused));
            }
            match call(auto, "image_edit", &json!({ "doc": doc, "page": page, "image": image, "action": "ungroup" })) {
                Ok(_) => done = done.saturating_add(1),
                Err(reason) => refused.push(reason),
            }
        }
        if done == before {
            break;
        }
    }
    Ok((done, refused))
}

/// `file-ungrouped.pdf` beside the input, numbered when that name is taken: never overwrites.
fn free_output(input: &Path) -> Result<PathBuf, String> {
    let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "document".to_string());
    let free = (1..=MAX_OUTPUT_NAMES)
        .map(|n| if n == 1 { format!("{stem}-ungrouped.pdf") } else { format!("{stem}-ungrouped-{n}.pdf") })
        .map(|name| input.with_file_name(name))
        .find(|path| !path.exists());
    free.ok_or_else(|| format!("too many ungrouped copies of {stem} already"))
}

/// Whether two renders are the same size and the same pixels. A render that failed counts as
/// different: the check could not show the page is unchanged.
fn same_look(before: Option<&(u32, u32, Pixels)>, after: Option<&(u32, u32, Pixels)>) -> bool {
    match (before, after) {
        (Some(a), Some(b)) => (a.0, a.1) == (b.0, b.1) && a.2.as_ref() == b.2.as_ref(),
        _ => false,
    }
}

fn ungroup_file(input: &Path) -> Result<Report, String> {
    let path = input.to_str().ok_or("this file's name can't be read as Unicode")?;
    let mut auto = Automation::new();
    let opened = call(&mut auto, "doc_open", &json!({ "path": path }))?;
    let doc = opened["doc"].as_u64().ok_or("the document did not open")?;
    if opened["editable"] == false {
        return Err(format!("this PDF can't be edited: {}", opened["read_only_reason"]));
    }
    let pages = opened["pages"].as_u64().unwrap_or(0);
    let mut report = Report::default();
    for page in 1..=pages {
        if form_numbers(&mut auto, doc, page)?.is_empty() {
            continue;
        }
        let page_number = usize::try_from(page).map_err(|e| e.to_string())?;
        let before = auto.render_pixels(doc, page_number, CHECK_SCALE).ok();
        let (done, refused) = ungroup_page(&mut auto, doc, page)?;
        report.ungrouped = report.ungrouped.saturating_add(done);
        report.refused.extend(refused.into_iter().map(|reason| (page, reason)));
        let after = auto.render_pixels(doc, page_number, CHECK_SCALE).ok();
        if done > 0 && !same_look(before.as_ref(), after.as_ref()) {
            report.changed_look.push(page);
        }
    }
    if report.ungrouped > 0 {
        let output = free_output(input)?;
        let target = output.to_str().ok_or("the new file's name can't be read as Unicode")?;
        call(&mut auto, "doc_save", &json!({ "doc": doc, "path": target }))?;
        report.output = Some(output);
    }
    Ok(report)
}

fn print_report(out: &mut impl Write, report: &Report) -> std::io::Result<()> {
    match &report.output {
        Some(path) => writeln!(out, "Ungrouped {} item(s). Saved {}", report.ungrouped, path.display())?,
        None => writeln!(out, "Nothing was ungrouped, so no new file was made.")?,
    }
    for (page, reason) in &report.refused {
        writeln!(out, "Page {page}: left as it was: {reason}")?;
    }
    for page in &report.changed_look {
        writeln!(out, "Page {page}: WARNING: this page may look different now. Compare it with the original.")?;
    }
    Ok(())
}

/// Keeps the window of a right-click run open until its message has been read.
fn wait_for_enter() {
    let _ = writeln!(std::io::stdout(), "\nPress Enter to close.");
    let _ = std::io::stdin().lock().read_line(&mut String::new());
}

/// What `args` (without the program name) ask for: the input and the viewer to open the result in.
fn parse_args(args: &[std::ffi::OsString]) -> Result<(PathBuf, Option<PathBuf>), String> {
    match args {
        [input] => Ok((PathBuf::from(input), None)),
        [input, flag, viewer] if flag == "--open" => Ok((PathBuf::from(input), Some(PathBuf::from(viewer)))),
        _ => Err("usage: pdfcraft-ungroup file.pdf [--open pdfcraft.exe]".to_string()),
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let (input, viewer) = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(std::io::stderr(), "{message}");
            return ExitCode::FAILURE;
        }
    };
    let interactive = viewer.is_some();
    let report = match ungroup_file(&input) {
        Ok(report) => report,
        Err(message) => {
            let _ = writeln!(std::io::stdout(), "Could not ungroup {}: {message}", input.display());
            if interactive {
                wait_for_enter();
            }
            return ExitCode::FAILURE;
        }
    };
    let _ = print_report(&mut std::io::stdout(), &report);
    let mut opened = true;
    if let (Some(viewer), Some(output)) = (&viewer, &report.output)
        && let Err(e) = Command::new(viewer).arg(output).spawn()
    {
        let _ = writeln!(std::io::stdout(), "Could not open it in {}: {e}", viewer.display());
        opened = false;
    }
    let clean = report.output.is_some() && report.refused.is_empty() && report.changed_look.is_empty();
    if interactive && !(clean && opened) {
        wait_for_enter();
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A one-page PDF from numbered objects: 1 catalog, 2 pages, 3 page, 4 content, then `rest`.
    fn pdf(page_content: &[u8], resources: &str, rest: &[Vec<u8>]) -> Vec<u8> {
        let mut objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 160] /Contents 4 0 R /Resources << {resources} >> >>").into_bytes(),
            stream("<<", page_content),
        ];
        objs.extend_from_slice(rest);
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(o);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
        out
    }

    fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
        [format!("{dict} /Length {} >>\nstream\n", data.len()).as_bytes(), data, b"\nendstream"].concat()
    }

    const FONT: &[u8] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>";
    const TEXT: &[u8] = b"BT /F1 12 Tf 1 0 0 1 20 120 Tm (Revenue) Tj 1 0 0 1 120 120 Tm (153,300.00) Tj ET";

    /// A whole page drawn by one Form, with its text inside it.
    fn text_in_a_form() -> Vec<u8> {
        pdf(
            b"q /Fm1 Do Q",
            "/XObject << /Fm1 5 0 R >>",
            &[stream("<< /Type /XObject /Subtype /Form /BBox [0 0 240 160] /Resources << /Font << /F1 6 0 R >> >>", TEXT), FONT.to_vec()],
        )
    }

    fn workdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pdfcraft-ungroup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// How many paragraphs of editable text page 1 has, and how it renders.
    fn page_text(path: &Path) -> (usize, Vec<u8>) {
        let mut auto = Automation::new();
        let doc = call(&mut auto, "doc_open", &json!({ "path": path.to_str().unwrap() })).unwrap()["doc"].as_u64().unwrap();
        let paragraphs = call(&mut auto, "text_paragraphs", &json!({ "doc": doc, "page": 1 })).unwrap();
        let (_, _, pixels) = auto.render_pixels(doc, 1, 1.0).unwrap();
        (paragraphs["paragraphs"].as_array().unwrap().len(), pixels.as_ref().to_vec())
    }

    #[test]
    fn a_page_drawn_by_one_form_gets_editable_text_and_the_same_pixels() {
        let dir = workdir("whole-page");
        let input = dir.join("report.pdf");
        let original = text_in_a_form();
        std::fs::write(&input, &original).unwrap();
        let (paragraphs, before) = page_text(&input);
        assert_eq!(paragraphs, 0, "the text starts out locked inside the form");

        let report = ungroup_file(&input).unwrap();

        assert_eq!(report.ungrouped, 1);
        assert!(report.refused.is_empty(), "{report:?}");
        assert!(report.changed_look.is_empty(), "{report:?}");
        let output = report.output.unwrap();
        assert_eq!(output.file_name().unwrap(), "report-ungrouped.pdf");
        let (paragraphs, after) = page_text(&output);
        assert!(paragraphs >= 1, "the saved copy has page text");
        assert_eq!(before, after, "the saved copy draws the same pixels");
        assert_eq!(std::fs::read(&input).unwrap(), original, "the original is untouched");
    }

    #[test]
    fn a_form_inside_a_form_is_ungrouped_too() {
        let dir = workdir("nested");
        let input = dir.join("nested.pdf");
        let bytes = pdf(
            b"q /Fm1 Do Q",
            "/XObject << /Fm1 5 0 R >>",
            &[
                stream("<< /Type /XObject /Subtype /Form /BBox [0 0 240 160] /Resources << /XObject << /Fm2 6 0 R >> >>", b"q /Fm2 Do Q"),
                stream("<< /Type /XObject /Subtype /Form /BBox [0 0 240 160] /Resources << /Font << /F1 7 0 R >> >>", TEXT),
                FONT.to_vec(),
            ],
        );
        std::fs::write(&input, bytes).unwrap();
        let report = ungroup_file(&input).unwrap();
        assert_eq!(report.ungrouped, 2, "{report:?}");
        assert!(page_text(&report.output.unwrap()).0 >= 1);
    }

    #[test]
    fn a_form_that_draws_itself_stops() {
        let dir = workdir("cycle");
        let input = dir.join("cycle.pdf");
        let bytes = pdf(
            b"q /Fm1 Do Q",
            "/XObject << /Fm1 5 0 R >>",
            &[stream("<< /Type /XObject /Subtype /Form /BBox [0 0 240 160] /Resources << /XObject << /Fm1 5 0 R >> >>", b"q /Fm1 Do Q q /Fm1 Do Q")],
        );
        std::fs::write(&input, bytes).unwrap();
        // Any outcome is fine as long as it returns.
        let _ = ungroup_file(&input);
    }

    #[test]
    fn a_knockout_group_is_reported_and_left_alone() {
        let dir = workdir("knockout");
        let input = dir.join("knockout.pdf");
        let bytes = pdf(
            b"q /Fm1 Do Q",
            "/XObject << /Fm1 5 0 R >>",
            &[
                stream(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 240 160] /Group << /S /Transparency /K true >> /Resources << /Font << /F1 6 0 R >> >>",
                    TEXT,
                ),
                FONT.to_vec(),
            ],
        );
        std::fs::write(&input, bytes).unwrap();
        let report = ungroup_file(&input).unwrap();
        assert_eq!((report.ungrouped, report.refused.len()), (0, 1), "{report:?}");
        assert_eq!(report.output, None, "nothing changed, so no new file");
        assert!(!dir.join("knockout-ungrouped.pdf").exists());
    }

    #[test]
    fn a_page_without_forms_makes_no_file() {
        let dir = workdir("plain");
        let input = dir.join("plain.pdf");
        std::fs::write(&input, pdf(TEXT, "/Font << /F1 5 0 R >>", &[FONT.to_vec()])).unwrap();
        assert_eq!(ungroup_file(&input).unwrap(), Report::default());
    }

    #[test]
    fn an_existing_copy_is_never_overwritten() {
        let dir = workdir("names");
        let input = dir.join("report.pdf");
        std::fs::write(&input, text_in_a_form()).unwrap();
        std::fs::write(dir.join("report-ungrouped.pdf"), b"keep me").unwrap();
        let output = ungroup_file(&input).unwrap().output.unwrap();
        assert_eq!(output.file_name().unwrap(), "report-ungrouped-2.pdf");
        assert_eq!(std::fs::read(dir.join("report-ungrouped.pdf")).unwrap(), b"keep me");
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        let dir = workdir("garbage");
        let input = dir.join("garbage.pdf");
        std::fs::write(&input, b"not a pdf").unwrap();
        assert!(ungroup_file(&input).is_err());
        assert!(ungroup_file(&dir.join("missing.pdf")).is_err());
    }

    #[test]
    fn arguments_are_a_file_and_an_optional_viewer() {
        let os = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert!(parse_args(&os(&[])).is_err());
        assert!(parse_args(&os(&["a.pdf", "--open"])).is_err());
        assert_eq!(parse_args(&os(&["a.pdf"])).unwrap(), (PathBuf::from("a.pdf"), None));
        assert_eq!(parse_args(&os(&["a.pdf", "--open", "v.exe"])).unwrap(), (PathBuf::from("a.pdf"), Some(PathBuf::from("v.exe"))));
    }
}
