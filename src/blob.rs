//! Interface for low-level parsing of a devicetree blob.
//!
//! It consists of _blocks_, most importantly the struct block, which in turn is
//! made of [`Token`]s. You can either iterate over them directly using
//! [`Devicetree::next_token`] or make use of [`Node`]s and [`Property`]s.

pub mod node;
mod token;

pub use node::Node;
pub use token::*;

#[cfg(feature = "alloc")]
use alloc::{boxed::Box, vec::Vec};
use core::{
	fmt::{self, Debug, Display, Formatter, Write},
	mem::{self, size_of, size_of_val},
	ptr::NonNull,
	slice,
};

use crate::{util, DeserializeNode, DeserializeProperty, NodeContext, Path, ReserveEntries};

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
	BlockOutOfBounds,
	IncompatibleVersion,
	InvalidPropertyHeader,
	InvalidRootNode,
	InvalidString,
	InvalidTotalsize,
	NoMagicSignature,
	UnalignedBlock,
	UnexpectedEnd,
	UnexpectedEndToken,
	UnexpectedEndNodeToken,
	UnknownToken,
}

impl Display for Error {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		use Error::*;

		let description = match *self {
			BlockOutOfBounds => "block out of bounds",
			IncompatibleVersion => "incompatible devicetree version",
			InvalidPropertyHeader => "invalid property header",
			InvalidRootNode => "invalid root node",
			InvalidString => "invalid string",
			InvalidTotalsize => "invalid totalsize field",
			NoMagicSignature => "no magic signature",
			UnalignedBlock => "unaligned block",
			UnexpectedEnd => "unexpected end",
			UnexpectedEndToken => "unexpected END token",
			UnexpectedEndNodeToken => "unexpected END_NODE token",
			UnknownToken => "unknown token",
		};
		write!(f, "devicetree blob error: {description}")
	}
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

impl From<Error> for crate::Error {
	fn from(err: Error) -> Self {
		Self::Blob(err)
	}
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

const DTB_ALIGN: usize = 8;
const DTB_MAGIC: u32 = 0xd00d_feed_u32.to_be();
const HEADER_SIZE: usize = size_of::<Header>();
const LAST_COMPATIBLE_VERSION: u32 = 16;
const MEMORY_RESERVATION_BLOCK_ALIGN: usize = 8;
const STRUCT_BLOCK_ALIGN: usize = 4;

#[repr(C, align(8))]
struct Header {
	magic: u32,
	totalsize: u32,
	off_dt_struct: u32,
	off_dt_strings: u32,
	off_mem_rsvmap: u32,
	version: u32,
	last_comp_version: u32,
	boot_cpuid_phys: u32,
	size_dt_strings: u32,
	size_dt_struct: u32,
}

/// Devicetree blob.
///
/// Alignment: 8 bytes
#[repr(transparent)]
pub struct Devicetree {
	blob: [u64],
}

impl Devicetree {
	/// Constructs a devicetree from a raw byte pointer. Mutable access is not
	/// required.
	///
	/// # Safety
	/// The pointer has to point to a valid DTB. Some safety checks are done
	/// nonetheless. Alignment is not checked.
	pub unsafe fn from_ptr(ptr: NonNull<u64>) -> Result<&'static Self> {
		let ptr: *const u64 = ptr.as_ptr();
		let blob = slice::from_raw_parts(ptr, HEADER_SIZE / DTB_ALIGN);
		Self::check_magic(blob)?;

		let size = Self::totalsize(blob) as usize;
		if size < HEADER_SIZE || usize::overflowing_add(ptr as usize, size).1 {
			// the buffer is too short or wraps around
			return Err(Error::InvalidTotalsize);
		}

		// idk if its allowed, but sometimes the dtb's length is not divisible by 8
		let slice_len = if size % 8 == 0 {
			size / DTB_ALIGN
		} else {
			(size + 7) / DTB_ALIGN
		};
		Self::from_slice_internal(slice::from_raw_parts(ptr, slice_len))
	}

	/// Constructs a devicetree from a slice containing a DTB.
	///
	/// If you only have a `&[u8]` value, consider using
	/// [`bytemuck::try_cast_slice`][try_cast_slice].
	///
	/// [try_cast_slice]: https://docs.rs/bytemuck/1/bytemuck/fn.try_cast_slice.html
	pub fn from_slice(blob: &[u64]) -> Result<&Self> {
		Self::safe_checks(blob)?;
		unsafe { Self::from_slice_internal(blob) }
	}

	/// Constructs a devicetree from a vec containing a DTB.
	///
	/// If you only have a vector of `u8`s, you are out of luck. Either use
	/// [`Devicetree::from_unaligned`], which will copy your vector, or
	/// [`Devicetree::from_slice`], which will return a reference.
	#[cfg(feature = "alloc")]
	pub fn from_vec(blob: Vec<u64>) -> Result<Box<Self>> {
		Self::safe_checks(&blob)?;
		let tree = unsafe { Self::from_box_unchecked(blob.into_boxed_slice()) };
		tree.late_checks()?;
		Ok(tree)
	}

	#[cfg(feature = "alloc")]
	unsafe fn from_box_unchecked(blob: Box<[u64]>) -> Box<Self> {
		Box::from_raw(Box::into_raw(blob) as *mut Devicetree)
	}

	/// Constructs a devicetree from an unaligned slice containing a DTB.
	#[cfg(feature = "alloc")]
	pub fn from_unaligned(blob: &[u8]) -> Result<Box<Self>> {
		let capacity_floor = blob.len() / DTB_ALIGN;
		let capacity = if blob.len() % DTB_ALIGN != 0 {
			capacity_floor + 1
		} else {
			capacity_floor
		};
		let mut aligned_blob: Vec<u64> = Vec::with_capacity(capacity);

		unsafe {
			if capacity_floor != capacity {
				*aligned_blob.as_mut_ptr().add(capacity_floor) = 0;
			}
			core::ptr::copy_nonoverlapping(
				blob.as_ptr(),
				aligned_blob.as_mut_ptr() as *mut u8,
				blob.len(),
			);
			aligned_blob.set_len(capacity);
		}

		Self::from_vec(aligned_blob)
	}

	/// # Safety
	/// `size_of_val(blob) >= HEADER_SIZE`
	unsafe fn from_slice_internal(blob: &[u64]) -> Result<&Self> {
		let tree: &Self = mem::transmute(blob);
		tree.late_checks()?;
		Ok(tree)
	}

	fn safe_checks(blob: &[u64]) -> Result<()> {
		if size_of_val(blob) < HEADER_SIZE {
			return Err(Error::UnexpectedEnd);
		}

		let size = unsafe {
			Self::check_magic(blob)?;
			Self::totalsize(blob)
		};
		// idk if its allowed, but sometimes the dtb's length is not divisible by 8
		if usize::checked_sub(size_of_val(blob), size as usize).map_or(true, |d| d >= 8) {
			return Err(Error::InvalidTotalsize);
		}
		Ok(())
	}

	/// Verifies the magic header of a devicetree blob.
	///
	/// # Safety
	/// `size_of_val(blob) >= HEADER_SIZE`
	unsafe fn check_magic(blob: &[u64]) -> Result<()> {
		if *(blob as *const _ as *const u32) != DTB_MAGIC {
			return Err(Error::NoMagicSignature);
		}

		Ok(())
	}

	/// The `totalsize` field of the devicetree blob header.
	///
	/// # Safety
	/// `size_of_val(blob) >= HEADER_SIZE`
	unsafe fn totalsize(blob: &[u64]) -> u32 {
		let header = blob as *const _ as *const Header;
		u32::from_be((*header).totalsize)
	}

	fn late_checks(&self) -> Result<()> {
		if self.header().last_comp_version != LAST_COMPATIBLE_VERSION.to_be() {
			return Err(Error::IncompatibleVersion);
		}
		Ok(())
	}

	fn header(&self) -> &Header {
		unsafe { &*(self as *const _ as *const Header) }
	}

	/// The devicetree blob specification version.
	///
	/// It has been at 17 ever since version 0.1 of the spec.
	pub fn version(&self) -> u32 {
		u32::from_be(self.header().version)
	}

	/// The last version compatible with this devicetree blob's version.
	///
	/// Currently required to be 16.
	pub fn last_comp_version(&self) -> u32 {
		LAST_COMPATIBLE_VERSION
	}

	/// The physical ID of the system's boot CPU.
	pub fn boot_cpuid_phys(&self) -> u32 {
		u32::from_be(self.header().boot_cpuid_phys)
	}

	/// The blob data as a `u8` slice.
	pub fn blob_u8(&self) -> &[u8] {
		unsafe { slice::from_raw_parts(self as *const _ as *const u8, size_of_val(self)) }
	}
	/// The blob data as a `u32` slice.

	pub fn blob_u32(&self) -> &[u32] {
		unsafe { slice::from_raw_parts(self as *const _ as *const u32, size_of_val(self) / 4) }
	}

	/// The blob data as a `u64` slice.
	pub fn blob(&self) -> &[u64] {
		&self.blob
	}

	/// The blob data of the struct block. Always 4-byte aligned.
	pub fn struct_blob(&self) -> Result<&[u32]> {
		let header = self.header();
		let offset = u32::from_be(header.off_dt_struct) as usize;
		let len = u32::from_be(header.size_dt_struct) as usize;
		if offset % STRUCT_BLOCK_ALIGN != 0 || len % STRUCT_BLOCK_ALIGN != 0 {
			return Err(Error::UnalignedBlock);
		}

		util::slice_get_with_len(
			self.blob_u32(),
			offset / STRUCT_BLOCK_ALIGN,
			len / STRUCT_BLOCK_ALIGN,
		)
		.ok_or(Error::BlockOutOfBounds)
	}

	/// The blob data of the strings block.
	pub fn strings_blob(&self) -> Result<&[u8]> {
		let header = self.header();
		let offset = u32::from_be(header.off_dt_strings) as usize;
		let len = u32::from_be(header.size_dt_strings) as usize;
		util::slice_get_with_len(self.blob_u8(), offset, len).ok_or(Error::BlockOutOfBounds)
	}

	/// Gets a node from the struct block by (loosely-matching) path.
	/// Try using [`Self::get_node_strict`] instead.
	///
	/// Doesn't respect `aliases`.
	/// The components need not match the node names exactly; the unit address
	/// (the part starting with an `@`) can be left out. If it is, the node name
	/// has to be unambiguous.
	pub fn get_node<P: Path + ?Sized>(&self, path: &P) -> crate::Result<Option<Node<'_>>> {
		let components = path.as_components()?;
		let mut node = self.root_node()?;
		for name in components {
			let Some(n) = node.get_child(name)? else { return Ok(None) };
			node = n;
		}

		Ok(Some(node))
	}

	/// Gets a node from the struct block by path.
	///
	/// The components have to match the node names exactly; the unit address
	/// (the part starting with an `@`) cannot be left out.
	pub fn get_node_strict<P: Path + ?Sized>(&self, path: &P) -> crate::Result<Option<Node<'_>>> {
		let components = path.as_components()?;
		let mut node = self.root_node()?;
		for name in components {
			let Some(n) = node.get_child_strict(name)? else { return Ok(None) };
			node = n;
		}

		Ok(Some(node))
	}

	pub fn parse_root<'dtb, T: DeserializeNode<'dtb>>(&'dtb self) -> crate::Result<T> {
		T::deserialize(&self.root_node()?, NodeContext::default()).map(|(v, _)| v)
	}

	/// Iterates over the memory reservation block.
	pub fn reserve_entries(&self) -> Result<ReserveEntries<'_>> {
		let offset = u32::from_be(self.header().off_mem_rsvmap) as usize;
		if offset % MEMORY_RESERVATION_BLOCK_ALIGN != 0 {
			return Err(Error::UnalignedBlock);
		}

		Ok(ReserveEntries {
			blob: self
				.blob
				.get(offset / MEMORY_RESERVATION_BLOCK_ALIGN..)
				.ok_or(Error::BlockOutOfBounds)?,
		})
	}
}

impl Debug for Devicetree {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		f.debug_struct("Devicetree").finish()
	}
}

impl<'a> From<&'a Devicetree> for &'a [u64] {
	fn from(dt: &'a Devicetree) -> Self {
		&dt.blob
	}
}

#[cfg(feature = "alloc")]
impl<'a> From<&'a Devicetree> for Box<Devicetree> {
	fn from(dt: &'a Devicetree) -> Self {
		unsafe { Devicetree::from_box_unchecked(dt.blob.into()) }
	}
}

/// A property contained in a [`Node`].
#[derive(Clone, Copy)]
pub struct Property<'dtb> {
	name_blob: &'dtb [u8],
	value: &'dtb [u8],
}

impl<'dtb> Property<'dtb> {
	/// The property's name.
	///
	/// # Errors
	/// Fails if the string is invalid.
	pub fn name(self) -> Result<&'dtb str> {
		util::get_c_str(self.name_blob)
	}

	/// The property's value.
	pub fn value(self) -> &'dtb [u8] {
		debug_assert_eq!(self.value.as_ptr() as usize % 4, 0);
		self.value
	}

	/// Parses the value. Equivalent to `DeserializeProperty` except that the
	/// default [`NodeContext`] is used.
	pub fn contextless_parse<T: DeserializeProperty<'dtb>>(self) -> crate::Result<T> {
		T::deserialize(self, NodeContext::default())
	}
}

impl<'dtb> Debug for Property<'dtb> {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		f.debug_struct("Property")
			.field("name", &self.name())
			.field("value", &self.value)
			.finish()
	}
}

impl<'dtb> Display for Property<'dtb> {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		const HEX_STRING: &[u8] = b"0123456789abcdef";

		struct HexArray<const N: usize>([u8; N]);

		impl<const N: usize> Display for HexArray<N> {
			fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
				let mut buf = [(0, 0); N];
				for (out, n) in core::iter::zip(&mut buf, self.0) {
					out.0 = HEX_STRING[n as usize >> 4];
					out.1 = HEX_STRING[n as usize & 0x0f];
				}
				unsafe {
					let buf = core::slice::from_raw_parts(&buf as *const _ as *const u8, N * 2);
					f.write_str(core::str::from_utf8_unchecked(buf))
				}
			}
		}

		f.write_str(self.name().map_err(|_| fmt::Error)?)?;
		if let [ref rest @ .., last_byte] = *self.value {
			f.write_str(" = ")?;
			let is_strings = last_byte == 0 && {
				// an all-zero value shouldn't be displayed as a bunch of empty strings
				let mut prev_was_printing_char = false;
				rest.iter().all(|&b| {
					match b {
						0 if prev_was_printing_char => prev_was_printing_char = false,
						b' '..=b'~' => prev_was_printing_char = true,
						_ => return false,
					}
					true
				}) && prev_was_printing_char
			};
			if is_strings {
				f.write_char('"')?;
				for &b in rest {
					if b == 0 {
						f.write_str("\", \"")?;
					} else {
						f.write_char(b as char)?;
					};
				}
				f.write_char('"')?;
			} else {
				f.write_char('[')?;
				let len = self.value.len();
				if len % 4 == 0 {
					for bytes in rest.chunks_exact(4) {
						write!(f, "{} ", HexArray(<[u8; 4]>::try_from(bytes).unwrap()))?;
					}
					// last byte is written below
					HexArray(<[u8; 3]>::try_from(&rest[len - 4..]).unwrap()).fmt(f)?;
				} else {
					for &b in rest {
						write!(f, "{} ", HexArray([b]))?;
					}
				}
				write!(f, "{}]", HexArray([last_byte]))?;
			}
		}
		f.write_char(';')
	}
}

/// Either a property or a node.
#[derive(Clone, Debug)]
pub enum Item<'dtb> {
	Property(Property<'dtb>),
	Child(Node<'dtb>),
}

impl<'dtb> Item<'dtb> {
	/// The property's name.
	///
	/// # Errors
	/// Fails if this is a property and the string is invalid.
	pub fn name(self) -> Result<&'dtb str> {
		match self {
			Self::Property(prop) => prop.name(),
			Self::Child(node) => Ok(node.name()),
		}
	}
}
