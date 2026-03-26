use std::cell::RefCell;

thread_local! {
    static SOURCE_LINE_STARTS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static SOURCE_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn set_current_source(source: &str) {
    let mut starts = Vec::with_capacity(source.len() / 24 + 1);
    starts.push(0);
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push((idx as u32).saturating_add(1));
        }
    }
    SOURCE_LINE_STARTS.with(|cell| {
        *cell.borrow_mut() = starts;
    });
    SOURCE_TEXT.with(|cell| {
        let mut text = cell.borrow_mut();
        text.clear();
        text.push_str(source);
    });
}

#[cfg(test)]
pub(crate) fn get_source_line_starts() -> Vec<u32> {
    SOURCE_LINE_STARTS.with(|starts| starts.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_source() {
        set_current_source("hello");
        assert_eq!(get_source_line_starts(), vec![0]);
    }

    #[test]
    fn multi_line_source() {
        set_current_source("a\nb\nc");
        assert_eq!(get_source_line_starts(), vec![0, 2, 4]);
    }

    #[test]
    fn empty_source() {
        set_current_source("");
        assert_eq!(get_source_line_starts(), vec![0]);
    }
}
