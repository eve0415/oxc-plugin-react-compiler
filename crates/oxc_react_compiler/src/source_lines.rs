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

fn offset_from_line_col(starts: &[u32], line: u32, column: u32) -> Option<usize> {
    let line_index = line.checked_sub(1)? as usize;
    let line_start = *starts.get(line_index)?;
    Some(line_start.saturating_add(column) as usize)
}

pub fn slice_line_col_range(
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
) -> Option<String> {
    SOURCE_LINE_STARTS.with(|starts_cell| {
        SOURCE_TEXT.with(|text_cell| {
            let starts = starts_cell.borrow();
            let text = text_cell.borrow();
            let start = offset_from_line_col(&starts, start_line, start_column)?;
            let end = offset_from_line_col(&starts, end_line, end_column)?;
            text.get(start..end).map(str::to_string)
        })
    })
}
