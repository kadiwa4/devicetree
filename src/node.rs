use core::hash::{Hash, Hasher};

use fallible_iterator::FallibleIterator;

use crate::{
	blob::{Cursor, Devicetree, Item, Node, Property, Token},
	Error, NodeContext, PushDeserializedNode, Result,
};

/// An iterator over the [`Item`]s ([`Property`]s and child [`Node`]s)
/// contained in a node.
///
/// Fused (see [`core::iter::FusedIterator`]).
///
/// In compliant devicetrees, the properties always come before the child nodes.
#[derive(Clone, Debug)]
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Items<'dtb> {
	dt: &'dtb Devicetree,
	at_depth: u32,
	pub(crate) cursor: Cursor,
}

impl<'dtb> Items<'dtb> {
	/// Creates a new iterator over the [`Item`]s contained in a node.
	///
	/// The cursor has to be inside the node.
	pub fn new(node: &Node<'dtb>, cursor: Cursor) -> Self {
		debug_assert!(node.contents <= cursor && node.contents.depth <= cursor.depth);
		Self {
			dt: node.dt,
			at_depth: node.contents.depth,
			cursor,
		}
	}

	/// The cursor has to be inside the node.
	pub fn set_cursor(&mut self, cursor: Cursor) {
		debug_assert!(self.at_depth <= cursor.depth);
		self.cursor = cursor;
	}

	/// A cursor pointing to the next [`Token`] after this node. Most expensive
	/// to determine if this iterator has not been advanced very much.
	pub fn end_cursor(mut self) -> Result<Cursor> {
		while self.next()?.is_some() {}
		Ok(self.cursor)
	}

	// Hidden because the exact behavior of this iterator could change.
	// Use `end_cursor` instead; this iterator is fused.
	#[doc(hidden)]
	pub fn _cursor_(self) -> Cursor {
		self.cursor
	}
}

impl<'dtb> FallibleIterator for Items<'dtb> {
	type Item = Item<'dtb>;
	type Error = Error;

	fn next(&mut self) -> Result<Option<Self::Item>, Self::Error> {
		while self.cursor.depth >= self.at_depth {
			let token_depth = self.cursor.depth;
			let Some(token) = self.dt.next_token(&mut self.cursor)? else { return Ok(None) };
			if token_depth == self.at_depth {
				return Ok(token.into_item());
			}
		}
		Ok(None)
	}
}

/// An iterator over the [`Property`]s contained in a node.
///
/// This is currently more efficient than filtering the [`Items`] manually.
#[derive(Clone, Debug)]
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Properties<'dtb> {
	dt: &'dtb Devicetree,
	cursor: Cursor,
}

impl<'dtb> Properties<'dtb> {
	/// Creates an iterator over the [`Property`]s contained in a node.
	///
	/// The cursor has to be inside the node.
	pub fn new(dt: &'dtb Devicetree, cursor: Cursor) -> Self {
		Self { dt, cursor }
	}

	/// Cursor pointing to the next [`Token`].
	pub fn cursor(&self) -> Cursor {
		self.cursor
	}

	/// Finds a contained property by name.
	pub fn find_by_name(
		&mut self,
		mut predicate: impl FnMut(&str) -> bool,
	) -> Result<Option<Property<'dtb>>> {
		self.find(|p| Ok(predicate(p.name()?)))
	}
}

impl<'dtb> FallibleIterator for Properties<'dtb> {
	type Item = Property<'dtb>;
	type Error = Error;

	fn next(&mut self) -> Result<Option<Self::Item>, Self::Error> {
		if let Some(Token::Property(prop)) = self.dt.next_token(&mut self.cursor)? {
			Ok(Some(prop))
		} else {
			Ok(None)
		}
	}
}

/// An iterator over the child [`Node`]s contained in a node.
///
/// This is currently not any more efficient than filtering the [`Items`]
/// manually.
///
/// Fused (see [`core::iter::FusedIterator`]).
#[derive(Clone, Debug)]
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct Children<'dtb>(Items<'dtb>);

impl<'dtb> Children<'dtb> {
	/// Creates an iterator over the child [`Node`]s contained in a node.
	///
	/// The cursor has to be inside the node.
	pub fn new(node: &Node<'dtb>, cursor: Cursor) -> Self {
		Self(Items::new(node, cursor))
	}

	/// The cursor has to be inside the node.
	pub fn set_cursor(&mut self, cursor: Cursor) {
		self.0.set_cursor(cursor);
	}

	/// A cursor pointing to the next [`Token`] after this node. Most expensive
	/// to determine if this iterator has not been advanced very much.
	pub fn end_cursor(self) -> Result<Cursor> {
		self.0.end_cursor()
	}

	/// Advances the iterator and passes the next node to the given closure.
	///
	/// The closure's second return value is a cursor pointing to the next token
	/// after the current node.
	pub fn walk_next<T>(
		&mut self,
		f: impl FnOnce(Node<'dtb>) -> Result<(T, Cursor)>,
	) -> Result<Option<T>> {
		let Some(child) = self.next()? else { return Ok(None) };
		let (ret, cursor) = f(child)?;
		self.0.set_cursor(cursor);
		Ok(Some(ret))
	}

	/// Searches for a node whose name satisfies the predicate.
	pub fn find_by_name(
		&mut self,
		mut predicate: impl FnMut(&str) -> bool,
	) -> Result<Option<Node<'dtb>>> {
		self.find(|n| Ok(predicate(n.name())))
	}

	/// Searches for a node whose split name satisfies the predicate.
	pub fn find_by_split_name(
		&mut self,
		mut predicate: impl FnMut(&str, Option<&str>) -> bool,
	) -> Result<Option<Node<'dtb>>> {
		self.find(|n| n.split_name().map(|(n, a)| predicate(n, a)))
	}

	/// Creates an iterator which uses a closure to determine if a node should
	/// be yielded. The predicate takes the node's split name as input.
	pub fn filter_by_split_name(
		&mut self,
		mut predicate: impl FnMut(&str, Option<&str>) -> bool,
	) -> fallible_iterator::Filter<&mut Self, impl FnMut(&Node<'dtb>) -> Result<bool>> {
		self.filter(move |n| n.split_name().map(|(n, a)| predicate(n, a)))
	}
}

impl<'dtb> FallibleIterator for Children<'dtb> {
	type Item = Node<'dtb>;
	type Error = Error;

	fn next(&mut self) -> Result<Option<Self::Item>, Self::Error> {
		self.0.find_map(|i| match i {
			Item::Property(_) => Ok(None),
			Item::Child(node) => Ok(Some(node)),
		})
	}
}

/// A range over all the child nodes with the same name, represented by cursors
/// to them.
///
/// Do not compare cursor ranges from different devicetrees.
/// Empty ranges do not belong to any node name/devicetree.
#[derive(Clone, Copy, Debug, Default, Eq, Hash)]
pub struct NamedRange<'dtb>(Option<(&'dtb str, BaseRange)>);

impl PartialEq for NamedRange<'_> {
	fn eq(&self, other: &Self) -> bool {
		if let (Self(Some((name0, base0))), Self(Some((name1, base1)))) = (*self, *other) {
			let ret = base0.first_offset == base1.first_offset && base0.len == base1.len;
			if ret {
				debug_assert_eq!(base0.depth, base1.depth);
				debug_assert_eq!(name0, name1);
			}
			ret
		} else {
			self.is_empty() == other.is_empty()
		}
	}
}

impl<'dtb> PushDeserializedNode<'dtb> for NamedRange<'dtb> {
	type Node = Node<'dtb>;

	fn push_node(&mut self, node: Self::Node, _cx: NodeContext<'_>) -> Result<()> {
		let Some((_, ref mut base)) = self.0 else {
			*self = Self::new_single(node)?;
			return Ok(());
		};
		let cursor = node.start_cursor();
		debug_assert_eq!(cursor.depth, base.depth);
		base.len += 1;
		Ok(())
	}
}

impl<'dtb> NamedRange<'dtb> {
	/// Default empty range.
	pub const EMPTY: Self = Self(None);

	/// Creates a new range spanning a single node.
	pub fn new_single(node: Node<'dtb>) -> Result<Self> {
		let cursor = node.start_cursor();
		Ok(Self(Some((
			node.split_name()?.0,
			BaseRange {
				depth: cursor.depth,
				first_offset: cursor.offset,
				len: 1,
			},
		))))
	}

	/// Cursor pointing to the first node's [`Token`].
	pub fn first(self) -> Option<Cursor> {
		self.0.map(|(_, b)| b.first())
	}

	pub fn len(self) -> usize {
		self.0.map_or(0, |(_, b)| b.len.try_into().unwrap())
	}

	pub fn is_empty(self) -> bool {
		self.0.is_none()
	}

	pub fn iter(self, dt: &'dtb Devicetree) -> NamedRangeIter<'dtb> {
		NamedRangeIter(self.0.map(|(filter_name, base)| NamedRangeIterInner {
			children: base.to_children(dt),
			filter_name,
			len: base.len,
		}))
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BaseRange {
	depth: u32,
	first_offset: u32,
	len: u32,
}

impl Hash for BaseRange {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.first_offset.hash(state);
		self.len.hash(state);
	}
}
impl BaseRange {
	fn first(self) -> Cursor {
		Cursor {
			depth: self.depth,
			offset: self.first_offset,
		}
	}

	fn to_children(self, dt: &Devicetree) -> Children<'_> {
		Children(Items {
			dt,
			at_depth: self.depth,
			cursor: Cursor {
				depth: self.depth,
				offset: self.first_offset,
			},
		})
	}
}

/// Iterator over the [`Node`]s in a named range.
/// Obtained from [`NamedRange::iter`].
#[derive(Clone, Debug, Default)]
#[must_use = "iterators are lazy and do nothing unless consumed"]
pub struct NamedRangeIter<'dtb>(Option<NamedRangeIterInner<'dtb>>);

impl<'dtb> FallibleIterator for NamedRangeIter<'dtb> {
	type Item = Node<'dtb>;
	type Error = Error;

	fn next(&mut self) -> Result<Option<Self::Item>, Self::Error> {
		let Self(Some(inner)) = self else { return Ok(None) };
		inner.len -= 1;
		let res = inner
			.children
			.find(|c| c.split_name().map(|(n, _)| n == inner.filter_name));
		if inner.len == 0 {
			*self = Self::EMPTY;
		}
		res
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		let len = self.remaining_len() as usize;
		(len, Some(len))
	}
}

impl<'dtb> NamedRangeIter<'dtb> {
	/// Default empty iterator.
	pub const EMPTY: Self = Self(None);

	pub fn remaining_len(&self) -> u32 {
		self.0.as_ref().map_or(0, |i| i.len)
	}
}

#[derive(Clone, Debug)]
struct NamedRangeIterInner<'dtb> {
	children: Children<'dtb>,
	filter_name: &'dtb str,
	len: u32,
}
