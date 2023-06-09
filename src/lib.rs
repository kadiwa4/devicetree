//! Parser for devicetree blobs compliant (hopefully) with version 0.4-rc1 of
//! [the specification][spec].
//!
//! [spec]: https://www.devicetree.org/specifications

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "alloc")]
extern crate alloc;

pub mod blob;
pub mod prop_value;

#[cfg(feature = "derive")]
pub use devicetree_derive::*;
pub use fallible_iterator;

use core::{
	any::Any,
	fmt::{self, Display, Formatter},
	iter,
	mem::size_of,
	slice,
};

use ascii::AsciiStr;
use fallible_iterator::FallibleIterator;

use blob::{Cursor, Item, Node, Property};

/// Any error caused by this crate.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
	/// Not produced by this crate.
	Unknown,
	Blob(blob::Error),
	InvalidNodeName,
	InvalidPath,
	TooManyCells,
	UnsuitableProperty,
}

impl Display for Error {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		use Error::*;

		let description = match self {
			Unknown => "unknown",
			Blob(err) => return Display::fmt(err, f),
			InvalidNodeName => "invalid node name",
			InvalidPath => "invalid path",
			TooManyCells => "too many cells",
			UnsuitableProperty => "unsuitable property",
		};
		write!(f, "devicetree error: {description}")
	}
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Number of 4-byte cells. This crate has an upper limit of 4 cells, but that
/// is not part of the spec.
pub type Cells = u8;
/// Numerical identifier for a node.
pub type Phandle = u32;

/// Absolute path of an item.
///
/// Either a slice of path components or a string with `/`-separated components.
/// A leading slash is required for a string path, other than that the path can
/// be empty.
pub trait Path {
	/// Iterator over the components of the path.
	type ComponentsIter<'a>: DoubleEndedIterator<Item = &'a str>
	where
		Self: 'a;

	/// An iterator over this path's components.
	fn as_components(&self) -> Result<Self::ComponentsIter<'_>>;
}

impl<'b> Path for [&'b str] {
	type ComponentsIter<'a> = iter::Copied<slice::Iter<'a, &'a str>>
	where
		Self: 'a;

	fn as_components(&self) -> Result<Self::ComponentsIter<'_>> {
		Ok(self.iter().copied())
	}
}

impl<'b, const N: usize> Path for [&'b str; N] {
	type ComponentsIter<'a> = iter::Copied<slice::Iter<'a, &'a str>>
	where
		Self: 'a;

	fn as_components(&self) -> Result<Self::ComponentsIter<'_>> {
		Ok(self.iter().copied())
	}
}

impl Path for str {
	type ComponentsIter<'a> = core::str::Split<'a, char>
	where
		Self: 'a;

	fn as_components(&self) -> Result<Self::ComponentsIter<'_>> {
		let mut components = self.split('/');
		if components.next() != Some("") {
			return Err(Error::InvalidPath);
		}
		if self.len() == 1 {
			// for the root node, the iterator should be empty
			components.next();
		}
		Ok(components)
	}
}

/// An entry from a blob's memory reservation block, obtained from
/// [`ReserveEntries`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReserveEntry {
	pub address: u64,
	pub size: u64,
}

/// An iterator over the [`ReserveEntry`] from a [`Devicetree`] blob's memory
/// reservation block.
///
/// [`Devicetree`]: blob::Devicetree
#[derive(Clone)]
pub struct ReserveEntries<'dtb> {
	blob: &'dtb [u64],
}

impl<'dtb> FallibleIterator for ReserveEntries<'dtb> {
	type Item = ReserveEntry;
	type Error = crate::Error;

	fn next(&mut self) -> crate::Result<Option<Self::Item>, Self::Error> {
		const RESERVE_ENTRY_U64_SIZE: usize = size_of::<ReserveEntry>() / 8;

		let blob = self
			.blob
			.get(..RESERVE_ENTRY_U64_SIZE)
			.ok_or(blob::Error::UnexpectedEnd)?;
		self.blob = &self.blob[RESERVE_ENTRY_U64_SIZE..];

		let address = blob[0];
		let size = blob[1];

		let entry = (address != 0 || size != 0).then(|| ReserveEntry {
			address: u64::from_be(address),
			size: u64::from_be(size),
		});
		Ok(entry)
	}
}

/// Holds information for deserialization.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct NodeContext<'a> {
	pub custom: Option<&'a dyn Any>,
	/// `#address-cells` property of the parent node.
	pub address_cells: Cells,
	/// `#size-cells` property of the parent node.
	pub size_cells: Cells,
}

impl Default for NodeContext<'_> {
	fn default() -> Self {
		Self {
			custom: None,
			address_cells: prop_value::AddressCells::default().0,
			size_cells: prop_value::SizeCells::default().0,
		}
	}
}

impl<'a> NodeContext<'a> {
	/// Helps you deserialize a node by creating the `NodeContext` for the child
	/// nodes for you.
	///
	/// `f_prop` is called with the name of each property and that property.
	///
	/// `f_child` is called with each node, the new `NodeContext` and a mutable
	/// reference to a cursor. The cursor points to that child node, but the
	/// function has to replace it with a cursor which points to next token
	/// after that child.
	pub fn deserialize_node<'dtb>(
		self,
		blob_node: &Node<'dtb>,
		mut f_prop: impl FnMut(&'dtb str, Property<'dtb>) -> Result<()>,
		mut f_child: impl FnMut(Node<'dtb>, Self, &mut Cursor) -> Result<()>,
	) -> Result<Cursor> {
		let mut child_cx = Self {
			custom: self.custom,
			..Self::default()
		};

		let mut items = blob_node.items();
		while let Some(item) = items.next()? {
			match item {
				Item::Property(prop) => {
					let name = prop.name()?;
					match name {
						"#address-cells" => {
							child_cx.address_cells =
								prop_value::AddressCells::deserialize(prop, self)?.0
						}
						"#size-cells" => {
							child_cx.size_cells = prop_value::SizeCells::deserialize(prop, self)?.0
						}
						_ => (),
					}
					f_prop(name, prop)?;
				}
				Item::Child(node) => {
					f_child(node, child_cx, &mut items.cursor)?;
				}
			}
		}
		Ok(items.cursor)
	}
}

/// Types that can be parsed from a devicetree property.
pub trait DeserializeProperty<'dtb>: Sized {
	/// Parses a devicetree property into this type.
	fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self>;
}

impl<'dtb> DeserializeProperty<'dtb> for Property<'dtb> {
	fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
		Ok(blob_prop)
	}
}

impl<'dtb> DeserializeProperty<'dtb> for () {
	fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
		if !blob_prop.value().is_empty() {
			return Err(Error::UnsuitableProperty);
		}
		Ok(())
	}
}

impl<'dtb> DeserializeProperty<'dtb> for bool {
	fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
		if !blob_prop.value().is_empty() {
			return Err(Error::UnsuitableProperty);
		}
		Ok(true)
	}
}

impl<'dtb> DeserializeProperty<'dtb> for &'dtb [u8] {
	fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
		Ok(blob_prop.value())
	}
}

impl<'dtb> DeserializeProperty<'dtb> for &'dtb [u32] {
	fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
		let val = blob_prop.value();
		if val.len() % 4 != 0 {
			return Err(Error::UnsuitableProperty);
		}
		Ok(unsafe { slice::from_raw_parts(val as *const _ as *const u32, val.len() / 4) })
	}
}

impl<'dtb> DeserializeProperty<'dtb> for u32 {
	fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
		match blob_prop.value().try_into() {
			Ok(arr) => Ok(Self::from_be_bytes(arr)),
			Err(_) => Err(Error::UnsuitableProperty),
		}
	}
}

impl<'dtb> DeserializeProperty<'dtb> for u64 {
	fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
		match blob_prop.value().try_into() {
			Ok(arr) => Ok(Self::from_be_bytes(arr)),
			Err(_) => Err(Error::UnsuitableProperty),
		}
	}
}

impl<'dtb> DeserializeProperty<'dtb> for &'dtb str {
	fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
		let [rest @ .., 0] = blob_prop.value() else { return Err(Error::UnsuitableProperty) };
		let bytes = AsciiStr::from_ascii(rest).map_err(|_| Error::UnsuitableProperty)?;
		Ok(bytes.as_str())
	}
}

impl<'dtb, T: DeserializeProperty<'dtb>> DeserializeProperty<'dtb> for Option<T> {
	fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
		T::deserialize(blob_prop, cx).map(Some)
	}
}

/// Types that can be parsed from a devicetree node.
pub trait DeserializeNode<'dtb>: Sized {
	/// Parses a devicetree node into this type.
	///
	/// The second return value is a cursor pointing to the next token after the
	/// node.
	fn deserialize(blob_node: &Node<'dtb>, cx: NodeContext<'_>) -> Result<(Self, Cursor)>;
}

impl<'dtb> DeserializeNode<'dtb> for Node<'dtb> {
	fn deserialize(blob_node: &Node<'dtb>, _cx: NodeContext<'_>) -> Result<(Self, Cursor)> {
		Ok((blob_node.clone(), blob_node.end_cursor()?))
	}
}

impl<'dtb> DeserializeNode<'dtb> for Cursor {
	fn deserialize(blob_node: &Node<'dtb>, _cx: NodeContext<'_>) -> Result<(Self, Cursor)> {
		Ok((blob_node.start_cursor(), blob_node.end_cursor()?))
	}
}

impl<'dtb, T: DeserializeNode<'dtb>> DeserializeNode<'dtb> for Option<T> {
	fn deserialize(blob_node: &Node<'dtb>, cx: NodeContext<'_>) -> Result<(Self, Cursor)> {
		T::deserialize(blob_node, cx).map(|(v, c)| (Some(v), c))
	}
}

/// Types that can collect several devicetree nodes.
pub trait PushDeserializedNode<'dtb> {
	type Node: DeserializeNode<'dtb>;

	/// Appends a node to the back of the collection.
	///
	/// This function has to be called with the nodes in the same order as they
	/// appear in the devicetree, without skipping any qualified ones.
	fn push_node(&mut self, node: Self::Node, _cx: NodeContext<'_>) -> Result<()>;
}

#[cfg(feature = "alloc")]
mod alloc_impl {
	use alloc::{boxed::Box, string::String, vec::Vec};

	use ::fallible_iterator::FallibleIterator;

	use crate::{blob::Item, prop_value::RegBlock, *};

	impl Path for [String] {
		type ComponentsIter<'a> = iter::Map<slice::Iter<'a, String>, fn(&String) -> &str>
		where
			Self: 'a;

		fn as_components(&self) -> Result<Self::ComponentsIter<'_>> {
			Ok(self.iter().map(String::as_str))
		}
	}

	impl<const N: usize> Path for [String; N] {
		type ComponentsIter<'a> = iter::Map<slice::Iter<'a, String>, fn(&String) -> &str>
		where
			Self: 'a;

		fn as_components(&self) -> Result<Self::ComponentsIter<'_>> {
			Ok(self.iter().map(String::as_str))
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Vec<u8> {
		fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
			Ok(blob_prop.value().to_vec())
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Box<[u8]> {
		fn deserialize(blob_prop: Property<'dtb>, _cx: NodeContext<'_>) -> Result<Self> {
			Ok(blob_prop.value().into())
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Vec<u32> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			<&[u32]>::deserialize(blob_prop, cx).map(<[u32]>::to_vec)
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Box<[u32]> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			<&[u32]>::deserialize(blob_prop, cx).map(Box::from)
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for String {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			<&str>::deserialize(blob_prop, cx).map(String::from)
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Box<str> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			<&str>::deserialize(blob_prop, cx).map(Box::from)
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Vec<&'dtb str> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			prop_value::Strings::deserialize(blob_prop, cx)?.collect()
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Vec<String> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			prop_value::Strings::deserialize(blob_prop, cx)?
				.map(|s| Ok(s.into()))
				.collect()
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Vec<Box<str>> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			prop_value::Strings::deserialize(blob_prop, cx)?
				.map(|s| Ok(s.into()))
				.collect()
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Box<[&'dtb str]> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			<Vec<&str>>::deserialize(blob_prop, cx).map(Vec::into_boxed_slice)
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Box<[String]> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			<Vec<String>>::deserialize(blob_prop, cx).map(Vec::into_boxed_slice)
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Box<[Box<str>]> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			<Vec<Box<str>>>::deserialize(blob_prop, cx).map(Vec::into_boxed_slice)
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Vec<RegBlock> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			prop_value::Reg::deserialize(blob_prop, cx).map(Vec::from_iter)
		}
	}

	impl<'dtb> DeserializeProperty<'dtb> for Box<[RegBlock]> {
		fn deserialize(blob_prop: Property<'dtb>, cx: NodeContext<'_>) -> Result<Self> {
			prop_value::Reg::deserialize(blob_prop, cx).map(Box::from_iter)
		}
	}

	impl<'dtb> DeserializeNode<'dtb> for Vec<Item<'dtb>> {
		fn deserialize(blob_node: &Node<'dtb>, _cx: NodeContext<'_>) -> Result<(Self, Cursor)> {
			let mut items = blob_node.items();
			Ok(((&mut items).collect::<Vec<_>>()?, items.cursor))
		}
	}

	impl<'dtb> DeserializeNode<'dtb> for Box<[Item<'dtb>]> {
		fn deserialize(blob_node: &Node<'dtb>, _cx: NodeContext<'_>) -> Result<(Self, Cursor)> {
			let mut items = blob_node.items();
			Ok(((&mut items).collect::<Box<[_]>>()?, items.cursor))
		}
	}

	impl<'dtb, A: DeserializeNode<'dtb>> PushDeserializedNode<'dtb> for Vec<A> {
		type Node = A;

		fn push_node(&mut self, node: Self::Node, _cx: NodeContext<'_>) -> Result<()> {
			self.push(node);
			Ok(())
		}
	}
}

/// Utility functions.
pub mod util {
	use crate::*;

	/// Splits a node name as follows:
	/// ```text
	/// <node-name> [@ <unit-address>]
	/// ```
	///
	/// # Errors
	/// There cannot be more than one `@`.
	///
	/// # Examples
	/// ```
	/// # use devicetree::{util::split_node_name, Error};
	/// assert_eq!(split_node_name("compatible"), Ok(("compatible", None)));
	/// assert_eq!(split_node_name("clock@0"), Ok(("clock", Some("0"))));
	/// assert_eq!(split_node_name("a@b@2"), Err(Error::InvalidNodeName));
	/// ```
	pub fn split_node_name(name: &str) -> Result<(&str, Option<&str>)> {
		let mut parts = name.split('@');
		let node_name = parts.next().unwrap();
		let unit_address = parts.next();
		if parts.next().is_some() {
			return Err(Error::InvalidNodeName);
		}
		Ok((node_name, unit_address))
	}

	/// Parses the 4-byte cells of a property value.
	///
	/// Returns something only if `bytes` is long enough and the cell count is
	/// no bigger than 16 bytes. The 16-byte limit is not part of the spec.
	/// Defaults to 0 if zero cells are to be parsed.
	pub fn parse_cells(bytes: &mut &[u32], cells: Cells) -> Option<u128> {
		if cells > 4 {
			return None;
		}
		let mut value: u128 = 0;
		for &byte in bytes.get(..cells as usize)? {
			value = (value << 0x20) | u32::from_be(byte) as u128;
		}
		*bytes = &bytes[cells as usize..];
		Some(value)
	}

	pub(crate) fn parse_cells_back(bytes: &mut &[u32], cells: Cells) -> Option<u128> {
		if cells > 4 {
			return None;
		}
		let idx = usize::checked_sub(bytes.len(), cells as usize)?;
		let mut value: u128 = 0;
		for &byte in &bytes[idx..] {
			value = (value << 0x20) | u32::from_be(byte) as u128;
		}
		*bytes = &bytes[..idx];
		Some(value)
	}

	pub(crate) fn get_c_str(blob: &[u8]) -> Result<&str, blob::Error> {
		let mut iter = blob.split(|&b| b == 0);
		let blob = iter.next().unwrap();
		iter.next().ok_or(blob::Error::InvalidString)?;

		let bytes = AsciiStr::from_ascii(blob).map_err(|_| blob::Error::InvalidString)?;
		Ok(bytes.as_str())
	}

	/// Same as `<[_]>::get` with a range except that it takes a length, not an end
	/// offset.
	pub(crate) fn slice_get_with_len<T>(slice: &[T], offset: usize, len: usize) -> Option<&[T]> {
		slice.get(offset..offset + len)
	}
}
