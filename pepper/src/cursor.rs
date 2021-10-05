use std::{
    num::NonZeroU8,
    ops::{Drop, Index, IndexMut, RangeFrom, RangeFull},
};

use crate::{
    buffer::{BufferContent, CharDisplayDistances},
    buffer_position::{BufferPosition, BufferRange},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub anchor: BufferPosition,
    pub position: BufferPosition,
}

impl Cursor {
    pub const fn zero() -> Self {
        Self {
            anchor: BufferPosition::zero(),
            position: BufferPosition::zero(),
        }
    }

    pub fn to_range(&self) -> BufferRange {
        BufferRange::between(self.anchor, self.position)
    }

    pub fn insert(&mut self, range: BufferRange) {
        self.anchor = self.anchor.insert(range);
        self.position = self.position.insert(range);
    }

    pub fn delete(&mut self, range: BufferRange) {
        self.anchor = self.anchor.delete(range);
        self.position = self.position.delete(range);
    }
}

#[derive(Clone)]
pub struct CursorCollection {
    cursors: Box<[Cursor; Self::capacity()]>,
    len: u8,
    saved_display_distances: Box<[u32; Self::capacity()]>,
    saved_display_distances_len: u8,
    main_cursor_index: u8,
}

impl CursorCollection {
    pub const fn capacity() -> usize {
        u8::MAX as _
    }

    pub fn new() -> Self {
        Self {
            cursors: Box::new([Cursor::zero(); Self::capacity()]),
            len: 1,
            saved_display_distances: Box::new([0; Self::capacity()]),
            saved_display_distances_len: 0,
            main_cursor_index: 0,
        }
    }

    pub fn main_cursor_index(&self) -> usize {
        self.main_cursor_index as _
    }

    pub fn main_cursor(&self) -> &Cursor {
        &self.cursors[self.main_cursor_index as usize]
    }

    pub fn mut_guard(&mut self) -> CursorCollectionMutGuard {
        CursorCollectionMutGuard {
            inner: self,
            clear_display_distances: true,
        }
    }

    fn sort_and_merge(&mut self) {
        let main_cursor = self.cursors[self.main_cursor_index as usize];
        self.cursors[..self.len as usize].sort_unstable_by_key(|c| c.to_range().from);
        self.main_cursor_index = self.cursors[..self.len as usize]
            .binary_search_by_key(&main_cursor.position, |c| c.position)
            .unwrap_or(0) as _;

        let mut i = 0;
        while i < self.len {
            let mut range = self.cursors[i as usize].to_range();
            for j in ((i + 1)..self.len).rev() {
                let other_range = self.cursors[j as usize].to_range();
                if range.from <= other_range.from && other_range.from <= range.to {
                    range.to = range.to.max(other_range.to);

                    self.cursors
                        .copy_within((j + 1) as usize..self.len as usize, j as _);
                    self.len -= 1;

                    if j <= self.main_cursor_index {
                        self.main_cursor_index -= 1;
                    }
                }
            }

            let cursor = &mut self.cursors[i as usize];
            *cursor = if cursor.anchor <= cursor.position {
                Cursor {
                    anchor: range.from,
                    position: range.to,
                }
            } else {
                Cursor {
                    anchor: range.to,
                    position: range.from,
                }
            };

            i += 1;
        }
    }
}

impl Index<usize> for CursorCollection {
    type Output = Cursor;
    fn index(&self, index: usize) -> &Self::Output {
        &self.cursors[index as usize]
    }
}
impl Index<RangeFull> for CursorCollection {
    type Output = [Cursor];
    fn index(&self, _: RangeFull) -> &Self::Output {
        &self.cursors[..self.len as usize]
    }
}
impl Index<RangeFrom<usize>> for CursorCollection {
    type Output = [Cursor];
    fn index(&self, index: RangeFrom<usize>) -> &Self::Output {
        &self.cursors[index.start..self.len as _]
    }
}

pub struct CursorCollectionMutGuard<'a> {
    inner: &'a mut CursorCollection,
    clear_display_distances: bool,
}

impl<'a> CursorCollectionMutGuard<'a> {
    pub fn clear(&mut self) {
        self.inner.len = 0;
    }

    pub fn add(&mut self, cursor: Cursor) {
        if let Some(len) = self.inner.len.checked_add(1) {
            self.inner.cursors[self.inner.len as usize] = cursor;
            self.inner.main_cursor_index = self.inner.len;
            self.inner.len = len;
        }
    }

    pub fn save_display_distances(&mut self, buffer: &BufferContent, tab_size: NonZeroU8) {
        self.clear_display_distances = false;
        if self.inner.saved_display_distances_len == 0 {
            for c in &self.inner.cursors[..self.inner.len as usize] {
                let line = &buffer.line_at(c.position.line_index as _).as_str()
                    [..c.position.column_byte_index as usize];
                let distance = CharDisplayDistances::new(line, tab_size)
                    .last()
                    .map(|d| d.distance)
                    .unwrap_or(0);

                self.inner.saved_display_distances
                    [self.inner.saved_display_distances_len as usize] = distance as _;
                self.inner.saved_display_distances_len += 1;
            }
        }
    }

    pub fn get_saved_display_distance(&self, index: usize) -> Option<u32> {
        if index < self.inner.saved_display_distances_len as usize {
            Some(self.inner.saved_display_distances[index])
        } else {
            None
        }
    }

    pub fn set_main_cursor_index(&mut self, index: usize) {
        self.inner.main_cursor_index = index.min(CursorCollection::capacity()) as u8;
    }

    pub fn main_cursor(&mut self) -> &mut Cursor {
        &mut self.inner.cursors[self.inner.main_cursor_index as usize]
    }
}

impl<'a> Index<usize> for CursorCollectionMutGuard<'a> {
    type Output = Cursor;
    fn index(&self, index: usize) -> &Self::Output {
        &self.inner.cursors[index as usize]
    }
}
impl<'a> Index<RangeFull> for CursorCollectionMutGuard<'a> {
    type Output = [Cursor];
    fn index(&self, _: RangeFull) -> &Self::Output {
        &self.inner.cursors[..self.inner.len as usize]
    }
}
impl<'a> Index<RangeFrom<usize>> for CursorCollectionMutGuard<'a> {
    type Output = [Cursor];
    fn index(&self, index: RangeFrom<usize>) -> &Self::Output {
        &self.inner.cursors[index.start..self.inner.len as _]
    }
}

impl<'a> IndexMut<usize> for CursorCollectionMutGuard<'a> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.inner.cursors[index as usize]
    }
}
impl<'a> IndexMut<RangeFull> for CursorCollectionMutGuard<'a> {
    fn index_mut(&mut self, _: RangeFull) -> &mut Self::Output {
        &mut self.inner.cursors[..self.inner.len as usize]
    }
}
impl<'a> IndexMut<RangeFrom<usize>> for CursorCollectionMutGuard<'a> {
    fn index_mut(&mut self, index: RangeFrom<usize>) -> &mut Self::Output {
        &mut self.inner.cursors[index.start..self.inner.len as _]
    }
}

impl<'a> Drop for CursorCollectionMutGuard<'a> {
    fn drop(&mut self) {
        if self.inner.len == 0 {
            self.inner.cursors[0] = Cursor::zero();
            self.inner.len = 1;
            self.inner.main_cursor_index = 0;
        }

        self.inner.sort_and_merge();

        if self.clear_display_distances {
            self.inner.saved_display_distances_len = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_cursor() {
        let mut cursors = CursorCollection::new();
        assert_eq!(1, cursors[..].len());
        let main_cursor = *cursors.main_cursor();
        cursors.mut_guard().add(main_cursor);
        let mut cursors = cursors[..].iter();
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::zero(), cursor.position);
        assert_eq!(BufferPosition::zero(), cursor.anchor);
        assert!(cursors.next().is_none());

        let mut cursors = CursorCollection::new();
        let mut cursors_mut = cursors.mut_guard();
        cursors_mut[0].anchor = BufferPosition::line_col(2, 3);
        cursors_mut[0].position = cursors_mut[0].anchor;
        drop(cursors_mut);
        assert_eq!(1, cursors[..].len());
        cursors.mut_guard().add(Cursor {
            anchor: BufferPosition::line_col(2, 2),
            position: BufferPosition::line_col(2, 4),
        });
        let mut cursors = cursors[..].iter();
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(2, 2), cursor.anchor);
        assert_eq!(BufferPosition::line_col(2, 4), cursor.position);
        assert!(cursors.next().is_none());

        let mut cursors = CursorCollection::new();
        let mut cursors_mut = cursors.mut_guard();
        cursors_mut[0].anchor = BufferPosition::line_col(2, 2);
        cursors_mut[0].position = BufferPosition::line_col(2, 4);
        drop(cursors_mut);
        cursors.mut_guard().add(Cursor {
            anchor: BufferPosition::line_col(2, 2),
            position: BufferPosition::line_col(2, 2),
        });
        let mut cursors = cursors[..].iter();
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(2, 2), cursor.anchor);
        assert_eq!(BufferPosition::line_col(2, 4), cursor.position);
        assert!(cursors.next().is_none());

        let mut cursors = CursorCollection::new();
        let mut cursors_mut = cursors.mut_guard();
        cursors_mut[0].anchor = BufferPosition::line_col(2, 2);
        cursors_mut[0].position = BufferPosition::line_col(2, 3);
        drop(cursors_mut);
        cursors.mut_guard().add(Cursor {
            anchor: BufferPosition::line_col(2, 4),
            position: BufferPosition::line_col(2, 3),
        });
        let mut cursors = cursors[..].iter();
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(2, 2), cursor.anchor);
        assert_eq!(BufferPosition::line_col(2, 4), cursor.position);
        assert!(cursors.next().is_none());

        let mut cursors = CursorCollection::new();
        let mut cursors_mut = cursors.mut_guard();
        cursors_mut[0].anchor = BufferPosition::line_col(2, 4);
        cursors_mut[0].position = BufferPosition::line_col(2, 3);
        drop(cursors_mut);
        cursors.mut_guard().add(Cursor {
            anchor: BufferPosition::line_col(2, 3),
            position: BufferPosition::line_col(2, 2),
        });
        let mut cursors = cursors[..].iter();
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(2, 4), cursor.anchor);
        assert_eq!(BufferPosition::line_col(2, 2), cursor.position);
        assert!(cursors.next().is_none());
    }

    #[test]
    fn no_merge_cursor() {
        let mut cursors = CursorCollection::new();
        let mut cursors_mut = cursors.mut_guard();
        cursors_mut[0].anchor = BufferPosition::line_col(1, 0);
        cursors_mut[0].position = BufferPosition::line_col(1, 0);
        drop(cursors_mut);
        cursors.mut_guard().add(Cursor {
            anchor: BufferPosition::line_col(2, 0),
            position: BufferPosition::line_col(2, 0),
        });
        let mut cursors = cursors[..].iter();
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(1, 0), cursor.anchor);
        assert_eq!(BufferPosition::line_col(1, 0), cursor.position);
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(2, 0), cursor.anchor);
        assert_eq!(BufferPosition::line_col(2, 0), cursor.position);
        assert!(cursors.next().is_none());

        let mut cursors = CursorCollection::new();
        let mut cursors_mut = cursors.mut_guard();
        cursors_mut[0].anchor = BufferPosition::line_col(3, 2);
        cursors_mut[0].position = BufferPosition::line_col(3, 2);
        drop(cursors_mut);
        cursors.mut_guard().add(Cursor {
            anchor: BufferPosition::line_col(2, 2),
            position: BufferPosition::line_col(2, 2),
        });
        let mut cursors = cursors[..].iter();
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(2, 2), cursor.anchor);
        assert_eq!(BufferPosition::line_col(2, 2), cursor.position);
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(3, 2), cursor.anchor);
        assert_eq!(BufferPosition::line_col(3, 2), cursor.position);
        assert!(cursors.next().is_none());
    }

    #[test]
    fn move_and_merge_cursors() {
        let mut cursors = CursorCollection::new();
        let mut cursors_mut = cursors.mut_guard();
        cursors_mut[0].anchor = BufferPosition::line_col(0, 0);
        cursors_mut[0].position = BufferPosition::line_col(0, 0);
        drop(cursors_mut);
        cursors.mut_guard().add(Cursor {
            anchor: BufferPosition::line_col(1, 0),
            position: BufferPosition::line_col(1, 0),
        });
        cursors.mut_guard().add(Cursor {
            anchor: BufferPosition::line_col(2, 0),
            position: BufferPosition::line_col(2, 0),
        });
        assert_eq!(3, cursors[..].len());
        let mut cursors_mut = cursors.mut_guard();
        for c in &mut cursors_mut[..] {
            if c.position.line_index > 0 {
                c.position.line_index -= 1;
            }
            c.anchor = c.position;
        }
        drop(cursors_mut);
        let cursor = cursors.main_cursor();
        assert_eq!(BufferPosition::line_col(1, 0), cursor.anchor);
        assert_eq!(BufferPosition::line_col(1, 0), cursor.position);
        let mut cursors = cursors[..].iter();
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(0, 0), cursor.anchor);
        assert_eq!(BufferPosition::line_col(0, 0), cursor.position);
        let cursor = cursors.next().unwrap();
        assert_eq!(BufferPosition::line_col(1, 0), cursor.anchor);
        assert_eq!(BufferPosition::line_col(1, 0), cursor.position);
        assert!(cursors.next().is_none());
    }
}
