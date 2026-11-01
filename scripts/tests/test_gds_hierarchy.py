"""Bounded hierarchy counting; the real KLayout iterator comparison is separate."""
import sys
from pathlib import Path
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'signoff'))
from gds_hierarchy import shape_counts

class Layout:
    def __init__(self, records):
        self.records = records
    def layer_indexes(self):
        return [0]
    def cell(self, index):
        return Cell(self, index)

class Cell:
    def __init__(self, layout, index):
        self.layout, self.index = layout, index
    def cell_index(self):
        return self.index
    def each_inst(self):
        return (Instance(self.layout, child, count) for child, count in self.layout.records[self.index][1])
    def shapes(self, layer):
        return self
    def size(self):
        return self.layout.records[self.index][0]

class Instance:
    def __init__(self, layout, child, count):
        self.cell, self.count = layout.cell(child), count
    def size(self):
        return self.count
    def is_regular_array(self):
        return self.count != 1

class HierarchyTests(unittest.TestCase):
    def test_large_multiplicity_and_shared_subtrees(self):
        layout = Layout({0:(5, [(1,2),(2,1)]), 1:(0,[(2,3)]), 2:(7,[])})
        self.assertEqual(shape_counts(layout, layout.cell(0)), {0:54})
        layout = Layout({0:(0,[(1,1_000_000_000)]), 1:(2,[])})
        self.assertEqual(shape_counts(layout, layout.cell(0)), {0:2_000_000_000})
    def test_cycles_and_limits_are_errors(self):
        loop = Layout({0:(0,[(1,1)]),1:(0,[(0,1)])})
        with self.assertRaisesRegex(ValueError,'cyclic'):
            shape_counts(loop,loop.cell(0))
        good = Layout({0:(0,[(1,4)]),1:(2,[])})
        for cap in ({'max_count':7},{'max_edges':0},{'max_entries':0},{'max_work':1},{'max_references':0},{'max_cells':1}):
            with self.assertRaises(ValueError):
                shape_counts(good, good.cell(0), **cap)

if __name__ == '__main__': unittest.main()
