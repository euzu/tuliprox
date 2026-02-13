use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Display, Formatter, Result};

#[derive(Debug)]
pub struct DirectedGraph<K>
where
    K: Eq + std::hash::Hash + Clone + Display + Debug,
{
    adjacencies: HashMap<K, Vec<K>>,
}

impl<K> DirectedGraph<K>
where
    K: Eq + std::hash::Hash + Clone + Display + Debug,
{
    pub fn new() -> Self {
        Self {
            adjacencies: HashMap::new(),
        }
    }

    // Add a node to the graph, ignore if it already exists
    pub fn add_node(&mut self, node: &K) {
        self.adjacencies.entry(node.to_owned()).or_default();
    }

    // Add a directed edge to the graph, ignore if it already exists
    pub fn add_edge(&mut self, from: &K, to: &K) {
        match self.adjacencies.entry(from.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let edges = entry.get_mut();
                if !edges.contains(to) {
                    edges.push(to.to_owned());
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(vec![to.to_owned()]);
            }
        }
    }

    // Detect and return cycles in the graph
    pub fn find_cycles(&self) -> Vec<Vec<K>> {
        let mut visited = HashSet::new();
        let mut recursion_stack = Vec::with_capacity(128);
        let mut cycles = Vec::with_capacity(128);

        for node in self.adjacencies.keys() {
            if !visited.contains(node) {
                self.dfs_find_cycles(node, &mut visited, &mut recursion_stack, &mut cycles);
            }
        }

        cycles
    }

    // Depth-first search to find cycles and return them
    fn dfs_find_cycles(
        &self,
        node: &K,
        visited: &mut HashSet<K>,
        recursion_stack: &mut Vec<K>,
        cycles: &mut Vec<Vec<K>>,
    ) {
        visited.insert(node.clone());
        recursion_stack.push(node.clone());

        if let Some(neighbors) = self.adjacencies.get(node) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    self.dfs_find_cycles(neighbor, visited, recursion_stack, cycles);
                } else if recursion_stack.contains(neighbor) {
                    // Cycle detected; collect the cycle path
                    if let Some(cycle_start_index) =
                        recursion_stack.iter().position(|n| n == neighbor)
                    {
                        let cycle = recursion_stack[cycle_start_index..].to_vec();
                        cycles.push(cycle);
                    }
                }
            }
        }

        recursion_stack.pop();
    }

    pub fn has_cycle(&self) -> bool {
        let mut visited = HashSet::new();
        let mut recursion_stack = HashSet::new();

        for node in self.adjacencies.keys() {
            if !visited.contains(node) && self.dfs(node, &mut visited, &mut recursion_stack) {
                return true;
            }
        }
        false
    }

    // Depth-first search for cycle detection
    fn dfs(&self, node: &K, visited: &mut HashSet<K>, recursion_stack: &mut HashSet<K>) -> bool {
        if !visited.contains(node) {
            visited.insert(node.clone());
            recursion_stack.insert(node.clone());

            if let Some(neighbors) = self.adjacencies.get(node) {
                for neighbor in neighbors {
                    if (!visited.contains(neighbor) && self.dfs(neighbor, visited, recursion_stack))
                        || recursion_stack.contains(neighbor)
                    {
                        return true;
                    }
                }
            }
        }

        recursion_stack.remove(node);
        false
    }

    // Return all dependencies as a NodeDependencies struct if no cyclic dependencies exist
    pub fn get_dependencies(&self) -> Option<HashMap<K, Vec<K>>> {
        if self.has_cycle() {
            return None;
        }

        let mut dependencies = HashMap::new();
        for (node, adj_nodes) in &self.adjacencies {
            if !adj_nodes.is_empty() {
                dependencies.insert(node.clone(), adj_nodes.clone());
            }
        }
        if dependencies.is_empty() {
            return None;
        }
        Some(dependencies)
    }

    // Topological sort function
    pub fn topological_sort(&self) -> Option<Vec<K>> {
        let mut visited = HashSet::new();
        let mut temp_mark = HashSet::new();
        let mut result = Vec::new();

        for node in self.adjacencies.keys() {
            if !visited.contains(node)
                && !self.dfs_topological_sort(node, &mut visited, &mut temp_mark, &mut result)
            {
                return None; // Cycle detected
            }
        }

        Some(result)
    }

    // Helper function for topological sort using DFS
    fn dfs_topological_sort(
        &self,
        node: &K,
        visited: &mut HashSet<K>,
        temp_mark: &mut HashSet<K>,
        result: &mut Vec<K>,
    ) -> bool {
        if temp_mark.contains(node) {
            return false; // Cycle detected
        }

        if !visited.contains(node) {
            temp_mark.insert(node.clone());

            if let Some(neighbors) = self.adjacencies.get(node) {
                for neighbor in neighbors {
                    if !self.dfs_topological_sort(neighbor, visited, temp_mark, result) {
                        return false;
                    }
                }
            }

            temp_mark.remove(node);
            visited.insert(node.clone());
            result.push(node.clone());
        }

        true
    }
}

// Implement the Display trait for DirectedGraph
impl<K> Display for DirectedGraph<K>
where
    K: Eq + std::hash::Hash + Clone + Display + Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        for (node, edges) in &self.adjacencies {
            writeln!(f, "{node} -> {edges:?}")?;
        }
        Ok(())
    }
}

impl<K> Default for DirectedGraph<K>
where
    K: Eq + std::hash::Hash + Clone + Display + Debug,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::DirectedGraph;
    use std::collections::HashSet;

    fn are_vecs_equal(vec1: &Vec<&str>, vec2: &Vec<&str>) -> bool {
        let set1: HashSet<String> = vec1.iter().map(|s| (*s).to_string()).collect();
        let set2: HashSet<String> = vec2.iter().map(|s| (*s).to_string()).collect();

        set1 == set2
    }

    #[test]
    fn graph_cycle_test() {
        let mut graph = DirectedGraph::new();
        graph.add_node(&"A");
        graph.add_node(&"A"); // Should be ignored
        graph.add_node(&"B");
        graph.add_node(&"C");
        graph.add_node(&"D");

        graph.add_edge(&"A", &"B");
        graph.add_edge(&"A", &"B"); // Should be ignored
        graph.add_edge(&"B", &"C");
        graph.add_edge(&"C", &"D");
        graph.add_edge(&"D", &"B"); // Cyclic dependency: D -> B
        graph.add_edge(&"B", &"D"); // Cyclic dependency: B -> D

        let cycles = graph.find_cycles();
        assert!(!cycles.is_empty(), "No cyclic dependencies found.");
        // println!("{:?}", &cycles);
    }

    #[test]
    fn graph_dependency_test() {
        let mut graph = DirectedGraph::new();
        graph.add_node(&"A");
        graph.add_node(&"B");
        graph.add_node(&"C");
        graph.add_node(&"D");

        graph.add_edge(&"A", &"B");
        graph.add_edge(&"B", &"C");
        graph.add_edge(&"C", &"D");
        graph.add_edge(&"B", &"D");

        let cycles = graph.find_cycles();
        assert!(cycles.is_empty(), "cyclic dependencies found.");

        // {"B": ["C", "D"], "A": ["B"], "D": [], "C": ["D"]}
        let dependencies_opt = graph.get_dependencies();
        assert!(dependencies_opt.is_some(), "No dependencies found");
        let dependencies = dependencies_opt.unwrap();
        let a_deps = dependencies.get("A");
        assert!(a_deps.is_some(), "No dependencies for A found");
        assert!(
            are_vecs_equal(a_deps.unwrap(), &vec!["B"]),
            "Dependencies for A not match"
        );
        let b_deps = dependencies.get("B");
        assert!(b_deps.is_some(), "No dependencies for B found");
        assert!(
            are_vecs_equal(b_deps.unwrap(), &vec!["C", "D"]),
            "Dependencies for B not match"
        );
        let c_deps = dependencies.get("C");
        assert!(c_deps.is_some(), "No dependencies for C found");
        assert!(
            are_vecs_equal(c_deps.unwrap(), &vec!["D"]),
            "Dependencies for C not match"
        );
        let d_deps = dependencies.get("D");
        assert!(d_deps.is_none(), "No dependencies for D found");
    }

    #[test]
    fn graph_no_dependency_test() {
        let mut graph = DirectedGraph::new();
        graph.add_node(&"A");
        graph.add_node(&"B");
        graph.add_node(&"C");
        graph.add_node(&"D");
        let dependencies_opt = graph.get_dependencies();
        assert!(dependencies_opt.is_none(), "Dependencies found");
    }

    #[test]
    fn graph_topological_sort() {
        let mut graph = DirectedGraph::new();
        graph.add_node(&"A");
        graph.add_node(&"B");
        graph.add_node(&"C");
        graph.add_node(&"D");

        graph.add_edge(&"B", &"A");
        graph.add_edge(&"A", &"D");
        graph.add_edge(&"C", &"D");
        graph.add_edge(&"B", &"C");
        graph.add_edge(&"B", &"D");

        let sorted = graph.topological_sort();
        assert!(sorted.is_some(), "Could not sort");
        let sorted_list = sorted.unwrap();
        assert!(
            are_vecs_equal(&sorted_list, &vec!["D", "A", "C", "B"]),
            "sort order wrong"
        );
    }

    #[test]
    fn graph_test_2() {
        let mut graph = DirectedGraph::new();
        graph.add_node(&"A");
        graph.add_node(&"B");
        graph.add_node(&"C");
        graph.add_node(&"E");
        graph.add_node(&"F");
        graph.add_node(&"D");
        graph.add_node(&"G");

        graph.add_edge(&"C", &"A");
        graph.add_edge(&"C", &"B");
        graph.add_edge(&"D", &"C");
        graph.add_edge(&"D", &"E");
        graph.add_edge(&"G", &"C");
        graph.add_edge(&"G", &"F");

        let sorted = graph.topological_sort();
        assert!(sorted.is_some(), "Could not sort");
        let sorted_list = sorted.unwrap();
        assert!(
            are_vecs_equal(&sorted_list, &vec!["F", "B", "A", "C", "E", "D", "G"]),
            "sort order wrong"
        );

        // should be {"D": ["C", "E"], "G": ["C", "F"], "C": ["A", "B"]}
        assert!(graph.get_dependencies().is_some(), "No dependencies");
    }
}
