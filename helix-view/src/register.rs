use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    iter,
    num::NonZeroUsize,
};

use anyhow::Result;
use helix_core::NATIVE_LINE_ENDING;

use crate::{
    clipboard::{get_clipboard_provider, ClipboardProvider, ClipboardType},
    document::SCRATCH_BUFFER_NAME,
    Editor,
};

/// Standard registers store up to this many yanks worth of history.
/// Once a register hits this many yanks, it discards the oldest values to
/// make space for new yanks.
const MAX_REGISTER_HISTORY_LEN: usize = 100;

#[derive(Debug, Default)]
struct Register {
    /// The values held by the register.
    ///
    /// When yanking to a register, all values are pushed into this `VecDeque`. The length
    /// of those values is stored in `length`. So each yank is stored in this flat sequence,
    /// but this `VecDeque` holds the sequence of all yanks.
    ///
    /// This `VecDeque` should only be empty when constructing `Register` via `Default`, which
    /// we do in `Registers` for simplicity. (Note that it should be impossible to `write`
    /// with an empty set of values.)
    ///
    /// Yanks are stored least to most recent. Within each yank, values are stored in order.
    values: VecDeque<String>,
    /// The length of each yank into the register.
    lengths: VecDeque<NonZeroUsize>,
}

impl Register {
    fn latest_value(&self) -> Option<&String> {
        self.values.back()
    }

    fn values(&self) -> RegisterValues<'_> {
        let length = self.lengths.back().map(|len| len.get()).unwrap_or_default();
        RegisterValues::new(
            self.values
                .iter()
                .rev()
                .take(length)
                .rev()
                .map(|s| Cow::Borrowed(s.as_str())),
        )
    }

    fn write<I: IntoIterator<Item = String>>(&mut self, values: I) {
        // If the register is full, discard the oldest yank in history.
        if self.lengths.len() > MAX_REGISTER_HISTORY_LEN {
            // Greater than max length implies non-empty.
            let oldest_len = self.lengths.pop_front().unwrap();
            self.values.drain(..oldest_len.get());
        }

        let pre_yank_len = self.values.len();
        self.values.extend(values.into_iter());
        let yank_len = NonZeroUsize::new(self.values.len() - pre_yank_len)
            .expect("writes to registers must not be empty");
        self.lengths.push_back(yank_len);
    }

    fn push(&mut self, value: String) {
        self.values.push_back(value);
        if let Some(last_length) = self.lengths.back_mut() {
            *last_length = NonZeroUsize::new(last_length.get() + 1).unwrap();
        } else {
            self.lengths.push_back(NonZeroUsize::new(1).unwrap());
        }
    }
}

/// A key-value store for saving sets of values.
///
/// Each register corresponds to a `char`. Most chars can be used to store any set of
/// values but a few chars are "special registers". Special registers have unique
/// behaviors when read or written to:
///
/// * Black hole (`_`): all values read and written are discarded
/// * Selection indices (`#`): index number of each selection starting at 1
/// * Selection contents (`.`)
/// * Document path (`%`): filename of the current buffer
/// * System clipboard (`*`)
/// * Primary clipboard (`+`)
#[derive(Debug)]
pub struct Registers {
    /// The mapping of register to values.
    /// This contains non-special registers plus '+' and '*'.
    inner: HashMap<char, Register>,
    clipboard_provider: Box<dyn ClipboardProvider>,
    pub last_search_register: char,
}

impl Default for Registers {
    fn default() -> Self {
        Self {
            inner: Default::default(),
            clipboard_provider: get_clipboard_provider(),
            last_search_register: '/',
        }
    }
}

impl Registers {
    pub fn read<'a>(&'a self, name: char, editor: &'a Editor) -> Option<RegisterValues<'a>> {
        match name {
            '_' => Some(RegisterValues::new(iter::empty())),
            '#' => {
                let (view, doc) = current_ref!(editor);
                let selections = doc.selection(view.id).len();
                // ExactSizeIterator is implemented for Range<usize> but
                // not RangeInclusive<usize>.
                Some(RegisterValues::new(
                    (0..selections).map(|i| (i + 1).to_string().into()),
                ))
            }
            '.' => {
                let (view, doc) = current_ref!(editor);
                let text = doc.text().slice(..);
                Some(RegisterValues::new(doc.selection(view.id).fragments(text)))
            }
            '%' => {
                let doc = doc!(editor);

                let path = doc
                    .path()
                    .as_ref()
                    .map(|p| p.to_string_lossy())
                    .unwrap_or_else(|| SCRATCH_BUFFER_NAME.into());

                Some(RegisterValues::new(iter::once(path)))
            }
            '*' | '+' => Some(read_from_clipboard(
                self.clipboard_provider.as_ref(),
                self.inner.get(&name),
                match name {
                    '+' => ClipboardType::Clipboard,
                    '*' => ClipboardType::Selection,
                    _ => unreachable!(),
                },
            )),
            _ => self.inner.get(&name).map(Register::values),
        }
    }

    pub fn write<I: IntoIterator<Item = String>>(&mut self, name: char, values: I) -> Result<()> {
        match name {
            '_' => Ok(()),
            '#' | '.' | '%' => Err(anyhow::anyhow!("Register {name} does not support writing")),
            '*' | '+' => {
                self.inner.entry(name).or_default().write(values);
                let mut contents = String::new();
                for val in self.inner[&name].values() {
                    if !contents.is_empty() {
                        contents.push_str(NATIVE_LINE_ENDING.as_str());
                    }
                    contents.push_str(&val);
                }
                self.clipboard_provider.set_contents(
                    contents,
                    match name {
                        '+' => ClipboardType::Clipboard,
                        '*' => ClipboardType::Selection,
                        _ => unreachable!(),
                    },
                )?;
                Ok(())
            }
            _ => {
                self.inner.entry(name).or_default().write(values);
                Ok(())
            }
        }
    }

    pub fn push(&mut self, name: char, mut value: String) -> Result<()> {
        match name {
            '_' => Ok(()),
            '#' | '.' | '%' => Err(anyhow::anyhow!("Register {name} does not support pushing")),
            '*' | '+' => {
                let clipboard_type = match name {
                    '+' => ClipboardType::Clipboard,
                    '*' => ClipboardType::Selection,
                    _ => unreachable!(),
                };
                let contents = self.clipboard_provider.get_contents(clipboard_type)?;
                let register = self.inner.entry(name).or_default();

                if !contents_are_saved(register.values(), &contents) {
                    anyhow::bail!("Failed to push to register {name}: clipboard does not match register contents");
                }

                register.push(value.clone());
                if !contents.is_empty() {
                    value.push_str(NATIVE_LINE_ENDING.as_str());
                }
                value.push_str(&contents);
                self.clipboard_provider
                    .set_contents(value, clipboard_type)?;

                Ok(())
            }
            _ => {
                self.inner.entry(name).or_default().push(value);
                Ok(())
            }
        }
    }

    pub fn first<'a>(&'a self, name: char, editor: &'a Editor) -> Option<Cow<'a, str>> {
        self.read(name, editor).and_then(|mut values| values.next())
    }

    pub fn last<'a>(&'a self, name: char, editor: &'a Editor) -> Option<Cow<'a, str>> {
        self.read(name, editor).and_then(|values| values.last())
    }

    pub fn iter_preview(&self) -> impl Iterator<Item = (char, &str)> {
        self.inner
            .iter()
            .filter(|(name, _)| !matches!(name, '*' | '+'))
            .map(|(name, register)| {
                let preview = register
                    .latest_value()
                    .and_then(|s| s.lines().next())
                    .unwrap_or("<empty>");

                (*name, preview)
            })
            .chain(
                [
                    ('_', "<empty>"),
                    ('#', "<selection indices>"),
                    ('.', "<selection contents>"),
                    ('%', "<document path>"),
                    ('+', "<system clipboard>"),
                    ('*', "<primary clipboard>"),
                ]
                .iter()
                .copied(),
            )
    }

    pub fn clear(&mut self) {
        self.clear_clipboard(ClipboardType::Clipboard);
        self.clear_clipboard(ClipboardType::Selection);
        self.inner.clear()
    }

    pub fn remove(&mut self, name: char) -> bool {
        match name {
            '*' | '+' => {
                self.clear_clipboard(match name {
                    '+' => ClipboardType::Clipboard,
                    '*' => ClipboardType::Selection,
                    _ => unreachable!(),
                });
                self.inner.remove(&name);

                true
            }
            '_' | '#' | '.' | '%' => false,
            _ => self.inner.remove(&name).is_some(),
        }
    }

    fn clear_clipboard(&mut self, clipboard_type: ClipboardType) {
        if let Err(err) = self
            .clipboard_provider
            .set_contents("".into(), clipboard_type)
        {
            log::error!(
                "Failed to clear {} clipboard: {err}",
                match clipboard_type {
                    ClipboardType::Clipboard => "system",
                    ClipboardType::Selection => "primary",
                }
            )
        }
    }

    pub fn clipboard_provider_name(&self) -> Cow<str> {
        self.clipboard_provider.name()
    }
}

fn read_from_clipboard<'a>(
    provider: &dyn ClipboardProvider,
    register: Option<&'a Register>,
    clipboard_type: ClipboardType,
) -> RegisterValues<'a> {
    match provider.get_contents(clipboard_type) {
        Ok(contents) => {
            // If we're pasting the same values that we just yanked, re-use
            // the saved values. This allows pasting multiple selections
            // even when yanked to a clipboard.
            let Some(register) = register else {
                return RegisterValues::new(iter::once(contents.into()));
            };

            if contents_are_saved(register.values(), &contents) {
                register.values()
            } else {
                RegisterValues::new(iter::once(contents.into()))
            }
        }
        Err(err) => {
            log::error!(
                "Failed to read {} clipboard: {err}",
                match clipboard_type {
                    ClipboardType::Clipboard => "system",
                    ClipboardType::Selection => "primary",
                }
            );

            RegisterValues::new(iter::empty())
        }
    }
}

fn contents_are_saved(mut values: RegisterValues<'_>, mut contents: &str) -> bool {
    let line_ending = NATIVE_LINE_ENDING.as_str();

    match values.next() {
        Some(first) if contents.starts_with(&*first) => {
            contents = &contents[first.len()..];
        }
        None if contents.is_empty() => return true,
        _ => return false,
    }

    for value in values {
        if contents.starts_with(line_ending) && contents[line_ending.len()..].starts_with(&*value) {
            contents = &contents[line_ending.len() + value.len()..];
        } else {
            return false;
        }
    }

    true
}

// This is a wrapper of an iterator that is both double ended and exact size,
// and can return either owned or borrowed values. Regular registers can
// return borrowed values while some special registers need to return owned
// values.
pub struct RegisterValues<'a> {
    iter: Box<dyn DoubleEndedExactSizeIterator<Item = Cow<'a, str>> + 'a>,
}

impl<'a> RegisterValues<'a> {
    fn new(
        iter: impl DoubleEndedIterator<Item = Cow<'a, str>>
            + ExactSizeIterator<Item = Cow<'a, str>>
            + 'a,
    ) -> Self {
        Self {
            iter: Box::new(iter),
        }
    }
}

impl<'a> Iterator for RegisterValues<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<'a> DoubleEndedIterator for RegisterValues<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.next_back()
    }
}

impl<'a> ExactSizeIterator for RegisterValues<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
}

// Each RegisterValues iterator is both double ended and exact size. We can't
// type RegisterValues as `Box<dyn DoubleEndedIterator + ExactSizeIterator>`
// because only one non-auto trait is allowed in trait objects. So we need to
// create a new trait that covers both. `RegisterValues` wraps that type so that
// trait only needs to live in this module and not be imported for all register
// callsites.
trait DoubleEndedExactSizeIterator: DoubleEndedIterator + ExactSizeIterator {}

impl<I: DoubleEndedIterator + ExactSizeIterator> DoubleEndedExactSizeIterator for I {}
