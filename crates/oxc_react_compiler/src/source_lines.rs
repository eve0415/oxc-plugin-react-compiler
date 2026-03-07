use std::cell::RefCell;

thread_local! {
    static SOURCE_LINE_STARTS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
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
}

pub fn line_from_offset(offset: u32) -> Option<u32> {
    SOURCE_LINE_STARTS.with(|cell| {
        let starts = cell.borrow();
        if starts.is_empty() {
            return None;
        }
        let index = match starts.binary_search(&offset) {
            Ok(index) => index,
            Err(0) => 0,
            Err(index) => index - 1,
        };
        Some((index as u32).saturating_add(1))
    })
}
