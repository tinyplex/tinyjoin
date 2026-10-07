//! A small map from names to values, for the catalog's tables and indexes.

/// Values by name, in name order, kept as two vectors. A catalog holds few enough entries that
/// inserting into a vector costs less than a B-tree's bookkeeping, and the names are searched in
/// one function shared by every value type, where each `BTreeMap` value type compiles its own
/// copy of the map's code.
#[derive(Clone, Debug)]
pub(crate) struct NameMap<V> {
    names: Vec<String>,
    values: Vec<V>,
}

impl<V> Default for NameMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> NameMap<V> {
    pub(crate) const fn new() -> Self {
        Self {
            names: Vec::new(),
            values: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&V> {
        search(&self.names, name).ok().map(|at| &self.values[at])
    }

    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut V> {
        search(&self.names, name)
            .ok()
            .map(|at| &mut self.values[at])
    }

    /// Where `name` is among the names, or where it would go, for a caller that searches for a
    /// name once and then reads its value with [`Self::at`], or adds it with
    /// [`Self::insert_at`], without searching again. It only hands the search its arguments,
    /// so it is inlined: as a function of its own it was a second call for each statement a
    /// transaction stages.
    #[inline(always)]
    pub(crate) fn position(&self, name: &str) -> Result<usize, usize> {
        search(&self.names, name)
    }

    /// The value at `position`, where [`Self::position`] found its name.
    pub(crate) fn at(&self, position: usize) -> Option<&V> {
        self.values.get(position)
    }

    pub(crate) fn at_mut(&mut self, position: usize) -> Option<&mut V> {
        self.values.get_mut(position)
    }

    /// Adds `name` with `value` at `position`, where [`Self::position`] said a name the map
    /// lacks would go, and returns the value in its place.
    pub(crate) fn insert_at(&mut self, position: usize, name: String, value: V) -> &mut V {
        debug_assert!(position == 0 || self.names[position - 1] < name);
        debug_assert!(self.names.get(position).is_none_or(|next| name < *next));
        self.names.insert(position, name);
        self.values.insert_mut(position, value)
    }

    pub(crate) fn contains_key(&self, name: &str) -> bool {
        search(&self.names, name).is_ok()
    }

    /// Sets the value of `name`, returning the value it replaced.
    pub(crate) fn insert(&mut self, name: String, value: V) -> Option<V> {
        match search(&self.names, &name) {
            Ok(at) => Some(std::mem::replace(&mut self.values[at], value)),
            Err(at) => {
                self.names.insert(at, name);
                self.values.insert(at, value);
                None
            }
        }
    }

    pub(crate) fn remove(&mut self, name: &str) -> Option<V> {
        let at = search(&self.names, name).ok()?;
        self.names.remove(at);
        Some(self.values.remove(at))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.names.iter().zip(&self.values)
    }

    pub(crate) fn keys(&self) -> std::slice::Iter<'_, String> {
        self.names.iter()
    }

    pub(crate) fn values(&self) -> std::slice::Iter<'_, V> {
        self.values.iter()
    }

    pub(crate) fn values_mut(&mut self) -> std::slice::IterMut<'_, V> {
        self.values.iter_mut()
    }
}

impl<V> std::ops::Index<&str> for NameMap<V> {
    type Output = V;

    /// The value of `name`, which must be in the map.
    fn index(&self, name: &str) -> &V {
        self.get(name).expect("the name is in the map")
    }
}

impl<V> IntoIterator for NameMap<V> {
    type Item = (String, V);
    type IntoIter = std::iter::Zip<std::vec::IntoIter<String>, std::vec::IntoIter<V>>;

    fn into_iter(self) -> Self::IntoIter {
        self.names.into_iter().zip(self.values)
    }
}

impl<V> FromIterator<(String, V)> for NameMap<V> {
    fn from_iter<I: IntoIterator<Item = (String, V)>>(entries: I) -> Self {
        let mut map = Self::new();
        for (name, value) in entries {
            map.insert(name, value);
        }
        map
    }
}

/// Where `name` is in `names`, or where it would go.
fn search(names: &[String], name: &str) -> Result<usize, usize> {
    names.binary_search_by(|probe| probe.as_str().cmp(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_one_value_per_name_in_name_order() {
        let mut map = [("b", 2), ("a", 1), ("c", 3)]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect::<NameMap<_>>();
        assert_eq!(map.insert("b".into(), 20), Some(2));
        assert_eq!(map.insert("d".into(), 4), None);
        assert_eq!(map.len(), 4);
        assert_eq!(map.get("b"), Some(&20));
        assert_eq!(map.get("e"), None);
        *map.get_mut("a").unwrap() = 10;
        assert_eq!(map.remove("c"), Some(3));
        assert_eq!(map.remove("c"), None);
        assert!(!map.contains_key("c"));
        assert_eq!(
            map.iter()
                .map(|(name, value)| (name.as_str(), *value))
                .collect::<Vec<_>>(),
            [("a", 10), ("b", 20), ("d", 4)]
        );
        assert_eq!(map.keys().collect::<Vec<_>>(), ["a", "b", "d"]);
        assert_eq!(map.into_iter().map(|(_, value)| value).sum::<i32>(), 34);
    }

    #[test]
    fn reads_and_fills_the_place_a_search_found() {
        let mut map = [("b", 2), ("d", 4)]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect::<NameMap<_>>();
        // A name the map holds is found where its value is, which is read and written there.
        assert_eq!(map.position("b"), Ok(0));
        assert_eq!(map.position("d"), Ok(1));
        assert_eq!(map.at(1), Some(&4));
        *map.at_mut(0).unwrap() = 20;
        assert_eq!(map.get("b"), Some(&20));
        assert_eq!(map.at(2), None);
        assert_eq!(map.at_mut(2), None);
        // A name it lacks is reported where it would go: before every name, between two, and
        // after them all. Each is added there, and found there afterwards.
        for (name, value, position) in [("a", 1, 0), ("c", 3, 2), ("e", 5, 4)] {
            assert_eq!(map.position(name), Err(position));
            let added = map.insert_at(position, name.to_owned(), value);
            assert_eq!(*added, value);
            *added *= 10;
            assert_eq!(map.position(name), Ok(position));
            assert_eq!(map.at(position), Some(&(value * 10)));
        }
        assert_eq!(
            map.iter()
                .map(|(name, value)| (name.as_str(), *value))
                .collect::<Vec<_>>(),
            [("a", 10), ("b", 20), ("c", 30), ("d", 4), ("e", 50)]
        );
        // An empty map reports the first place for any name, and takes its first name there.
        let mut empty = NameMap::new();
        assert_eq!(empty.position("a"), Err(0));
        assert_eq!(empty.at(0), None::<&i32>);
        empty.insert_at(0, "a".to_owned(), 1);
        assert_eq!(empty.get("a"), Some(&1));
    }
}
