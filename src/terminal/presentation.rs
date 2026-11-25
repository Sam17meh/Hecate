use super::escape::{multiline, single_line};

pub(crate) fn section_title(title: &str) {
    println!("\n{}", section_line(title));
}

pub(crate) fn information(message: &str) {
    println!("{}", information_line(message));
}

pub(crate) fn success(message: &str) {
    println!("{}", success_line(message));
}

pub(crate) fn warning(message: &str) {
    println!("{}", warning_line(message));
}

pub(crate) fn error(message: &str) {
    eprintln!("{}", error_line(message));
}

pub(crate) fn multiline_block(indent: &str, text: &str) {
    let indent = safe_indent(indent);
    for line in multiline(text).split('\n') {
        println!("{indent}{line}");
    }
}

fn safe_indent(indent: &str) -> String {
    single_line(indent)
}

fn section_line(title: &str) -> String {
    single_line(title)
}

fn information_line(message: &str) -> String {
    format!("Info: {}", single_line(message))
}

fn success_line(message: &str) -> String {
    format!("Success: {}", single_line(message))
}

fn warning_line(message: &str) -> String {
    format!("Warning: {}", single_line(message))
}

fn error_line(message: &str) -> String {
    format!("HECATE: {}", single_line(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_plain_text_status_messages_without_ansi() {
        assert_eq!(section_line("Ready"), "Ready");
        assert_eq!(information_line("ready"), "Info: ready");
        assert_eq!(success_line("done"), "Success: done");
        assert_eq!(warning_line("review"), "Warning: review");
        assert_eq!(error_line("failed"), "HECATE: failed");
        assert_eq!(error_line("bad\u{1b}[2J"), "HECATE: bad\\x1b[2J");
        assert_eq!(safe_indent("  \u{1b}[2J\nforged"), "  \\x1b[2J\\nforged");
    }
}
