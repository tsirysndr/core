package sets

import (
	"slices"
	"testing"
	"testing/quick"
)

func TestNew(t *testing.T) {
	s := New[int]()
	if s.Len() != 0 {
		t.Errorf("New set should be empty, got length %d", s.Len())
	}
	if !s.IsEmpty() {
		t.Error("New set should be empty")
	}
}

func TestFromSlice(t *testing.T) {
	s := Collect(slices.Values([]int{1, 2, 3, 2, 1}))
	if s.Len() != 3 {
		t.Errorf("Expected length 3, got %d", s.Len())
	}
	if !s.Contains(1) || !s.Contains(2) || !s.Contains(3) {
		t.Error("Set should contain all unique elements from slice")
	}
}

func TestInsert(t *testing.T) {
	s := New[string]()

	if !s.Insert("hello") {
		t.Error("First insert should return true")
	}
	if s.Insert("hello") {
		t.Error("Duplicate insert should return false")
	}
	if s.Len() != 1 {
		t.Errorf("Expected length 1, got %d", s.Len())
	}
}

func TestRemove(t *testing.T) {
	s := Collect(slices.Values([]int{1, 2, 3}))

	if !s.Remove(2) {
		t.Error("Remove existing element should return true")
	}
	if s.Remove(2) {
		t.Error("Remove non-existing element should return false")
	}
	if s.Contains(2) {
		t.Error("Element should be removed")
	}
	if s.Len() != 2 {
		t.Errorf("Expected length 2, got %d", s.Len())
	}
}

func TestContains(t *testing.T) {
	s := Collect(slices.Values([]int{1, 2, 3}))

	if !s.Contains(1) {
		t.Error("Should contain 1")
	}
	if s.Contains(4) {
		t.Error("Should not contain 4")
	}
}

func TestClear(t *testing.T) {
	s := Collect(slices.Values([]int{1, 2, 3}))
	s.Clear()

	if !s.IsEmpty() {
		t.Error("Set should be empty after clear")
	}
	if s.Len() != 0 {
		t.Errorf("Expected length 0, got %d", s.Len())
	}
}

func TestIterator(t *testing.T) {
	s := Collect(slices.Values([]int{1, 2, 3}))
	var items []int

	for item := range s.All() {
		items = append(items, item)
	}

	slices.Sort(items)
	expected := []int{1, 2, 3}
	if !slices.Equal(items, expected) {
		t.Errorf("Expected %v, got %v", expected, items)
	}
}

func TestClone(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2, 3}))
	s2 := s1.Clone()

	if !s1.Equal(s2) {
		t.Error("Cloned set should be equal to original")
	}

	s2.Insert(4)
	if s1.Contains(4) {
		t.Error("Modifying clone should not affect original")
	}
}

func TestUnion(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2}))
	s2 := Collect(slices.Values([]int{2, 3}))

	result := Collect(s1.Union(s2))
	expected := Collect(slices.Values([]int{1, 2, 3}))

	if !result.Equal(expected) {
		t.Errorf("Expected %v, got %v", expected, result)
	}
}

func TestIntersection(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2, 3}))
	s2 := Collect(slices.Values([]int{2, 3, 4}))

	expected := Collect(slices.Values([]int{2, 3}))
	result := Collect(s1.Intersection(s2))

	if !result.Equal(expected) {
		t.Errorf("Expected %v, got %v", expected, result)
	}
}

func TestDifference(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2, 3}))
	s2 := Collect(slices.Values([]int{2, 3, 4}))

	expected := Collect(slices.Values([]int{1}))
	result := Collect(s1.Difference(s2))

	if !result.Equal(expected) {
		t.Errorf("Expected %v, got %v", expected, result)
	}
}

func TestSymmetricDifference(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2, 3}))
	s2 := Collect(slices.Values([]int{2, 3, 4}))

	expected := Collect(slices.Values([]int{1, 4}))
	result := Collect(s1.SymmetricDifference(s2))

	if !result.Equal(expected) {
		t.Errorf("Expected %v, got %v", expected, result)
	}
}

func TestSymmetricDifferenceCommutativeProperty(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2, 3}))
	s2 := Collect(slices.Values([]int{2, 3, 4}))

	result1 := Collect(s1.SymmetricDifference(s2))
	result2 := Collect(s2.SymmetricDifference(s1))

	if !result1.Equal(result2) {
		t.Errorf("Expected %v, got %v", result1, result2)
	}
}

func TestIsSubset(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2}))
	s2 := Collect(slices.Values([]int{1, 2, 3}))

	if !s1.IsSubset(s2) {
		t.Error("s1 should be subset of s2")
	}
	if s2.IsSubset(s1) {
		t.Error("s2 should not be subset of s1")
	}
}

func TestIsSuperset(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2, 3}))
	s2 := Collect(slices.Values([]int{1, 2}))

	if !s1.IsSuperset(s2) {
		t.Error("s1 should be superset of s2")
	}
	if s2.IsSuperset(s1) {
		t.Error("s2 should not be superset of s1")
	}
}

func TestIsDisjoint(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2}))
	s2 := Collect(slices.Values([]int{3, 4}))
	s3 := Collect(slices.Values([]int{2, 3}))

	if !s1.IsDisjoint(s2) {
		t.Error("s1 and s2 should be disjoint")
	}
	if s1.IsDisjoint(s3) {
		t.Error("s1 and s3 should not be disjoint")
	}
}

func TestEqual(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2, 3}))
	s2 := Collect(slices.Values([]int{3, 2, 1}))
	s3 := Collect(slices.Values([]int{1, 2}))

	if !s1.Equal(s2) {
		t.Error("s1 and s2 should be equal")
	}
	if s1.Equal(s3) {
		t.Error("s1 and s3 should not be equal")
	}
}

func TestCollect(t *testing.T) {
	s1 := Collect(slices.Values([]int{1, 2}))
	s2 := Collect(slices.Values([]int{2, 3}))

	unionSet := Collect(s1.Union(s2))
	if unionSet.Len() != 3 {
		t.Errorf("Expected union set length 3, got %d", unionSet.Len())
	}
	if !unionSet.Contains(1) || !unionSet.Contains(2) || !unionSet.Contains(3) {
		t.Error("Union set should contain 1, 2, and 3")
	}

	diffSet := Collect(s1.Difference(s2))
	if diffSet.Len() != 1 {
		t.Errorf("Expected difference set length 1, got %d", diffSet.Len())
	}
	if !diffSet.Contains(1) {
		t.Error("Difference set should contain 1")
	}
}

func TestPropertySingletonLen(t *testing.T) {
	f := func(item int) bool {
		single := Singleton(item)
		return single.Len() == 1
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyInsertIdempotent(t *testing.T) {
	f := func(s Set[int], item int) bool {
		clone := s.Clone()

		clone.Insert(item)
		firstLen := clone.Len()

		clone.Insert(item)
		secondLen := clone.Len()

		return firstLen == secondLen
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyUnionCommutative(t *testing.T) {
	f := func(s1 Set[int], s2 Set[int]) bool {
		union1 := Collect(s1.Union(s2))
		union2 := Collect(s2.Union(s1))
		return union1.Equal(union2)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyIntersectionCommutative(t *testing.T) {
	f := func(s1 Set[int], s2 Set[int]) bool {
		inter1 := Collect(s1.Intersection(s2))
		inter2 := Collect(s2.Intersection(s1))
		return inter1.Equal(inter2)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyCloneEquals(t *testing.T) {
	f := func(s Set[int]) bool {
		clone := s.Clone()
		return s.Equal(clone)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyIntersectionIsSubset(t *testing.T) {
	f := func(s1 Set[int], s2 Set[int]) bool {
		inter := Collect(s1.Intersection(s2))
		return inter.IsSubset(s1) && inter.IsSubset(s2)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyUnionIsSuperset(t *testing.T) {
	f := func(s1 Set[int], s2 Set[int]) bool {
		union := Collect(s1.Union(s2))
		return union.IsSuperset(s1) && union.IsSuperset(s2)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyDifferenceDisjoint(t *testing.T) {
	f := func(s1 Set[int], s2 Set[int]) bool {
		diff := Collect(s1.Difference(s2))
		return diff.IsDisjoint(s2)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertySymmetricDifferenceCommutative(t *testing.T) {
	f := func(s1 Set[int], s2 Set[int]) bool {
		symDiff1 := Collect(s1.SymmetricDifference(s2))
		symDiff2 := Collect(s2.SymmetricDifference(s1))
		return symDiff1.Equal(symDiff2)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyRemoveWorks(t *testing.T) {
	f := func(s Set[int], item int) bool {
		clone := s.Clone()
		clone.Insert(item)
		clone.Remove(item)
		return !clone.Contains(item)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyClearEmpty(t *testing.T) {
	f := func(s Set[int]) bool {
		s.Clear()
		return s.IsEmpty() && s.Len() == 0
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyIsSubsetReflexive(t *testing.T) {
	f := func(s Set[int]) bool {
		return s.IsSubset(s)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}

func TestPropertyDeMorganUnion(t *testing.T) {
	f := func(s1 Set[int], s2 Set[int], universe Set[int]) bool {
		// create a universe that contains both sets
		u := universe.Clone()
		for item := range s1.All() {
			u.Insert(item)
		}
		for item := range s2.All() {
			u.Insert(item)
		}

		// (A u B)' = A' n B'
		union := Collect(s1.Union(s2))
		complementUnion := Collect(u.Difference(union))

		complementS1 := Collect(u.Difference(s1))
		complementS2 := Collect(u.Difference(s2))
		intersectionComplements := Collect(complementS1.Intersection(complementS2))

		return complementUnion.Equal(intersectionComplements)
	}

	if err := quick.Check(f, nil); err != nil {
		t.Error(err)
	}
}
