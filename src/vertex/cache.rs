//! Vertex transform cache analysis and optimization

use crate::util::fill_slice;

#[derive(Default)]
pub struct VertexCacheStatistics {
	pub vertices_transformed: u32,
    pub warps_executed: u32,
	/// Transformed vertices / triangle count
	///
    /// Best case 0.5, worst case 3.0, optimum depends on topology
    pub acmr: f32,
	/// Transformed vertices / vertex count
	///
    /// Best case 1.0, worst case 6.0, optimum is 1.0 (each vertex is transformed once)
	pub atvr: f32,
}

/// Returns cache hit statistics using a simplified FIFO model.
///
/// Results may not match actual GPU performance.
pub fn analyze_vertex_cache(indices: &[u32], vertex_count: usize, cache_size: usize, warp_size: usize, primgroup_size: usize) -> VertexCacheStatistics {
	assert!(indices.len() % 3 == 0);
	assert!(cache_size >= 3);
	assert!(warp_size == 0 || warp_size >= 3);

	let mut result = VertexCacheStatistics::default();

	let mut warp_offset = 0;
	let mut primgroup_offset = 0;

	let mut cache_timestamps: Vec<u32> = vec![0; vertex_count];

	let mut timestamp = cache_size + 1;

	for i in (0..indices.len()).step_by(3) {
        let a = indices[i + 0] as usize;
        let b = indices[i + 1] as usize;
        let c = indices[i + 2] as usize;
		assert!(a < vertex_count && b < vertex_count && c < vertex_count);

		let ac = ((timestamp - cache_timestamps[a] as usize) > cache_size) as usize;
		let bc = ((timestamp - cache_timestamps[b] as usize) > cache_size) as usize;
		let cc = ((timestamp - cache_timestamps[c] as usize) > cache_size) as usize;

		// flush cache if triangle doesn't fit into warp or into the primitive buffer
		if (primgroup_size > 0 && primgroup_offset == primgroup_size) || (warp_size > 0 && warp_offset + ac + bc + cc > warp_size) {
			result.warps_executed += (warp_offset > 0) as u32;

			warp_offset = 0;
			primgroup_offset = 0;

			// reset cache
			timestamp += cache_size + 1;
		}

		// update cache and add vertices to warp
		for j in 0..3 {
			let index = indices[i + j] as usize;

			if timestamp - cache_timestamps[index] as usize > cache_size {
                cache_timestamps[index] = timestamp as u32;
                timestamp += 1;
				result.vertices_transformed += 1;
				warp_offset += 1;
			}
		}

		primgroup_offset += 1;
	}

    let unique_vertex_count = cache_timestamps.iter().filter(|t| **t > 0).count();

	result.warps_executed += (warp_offset > 0) as u32;

	result.acmr = if indices.len() == 0 { 0.0 } else { result.vertices_transformed as f32 / (indices.len() as f32 / 3.0) };
	result.atvr = if unique_vertex_count == 0 { 0.0 } else { result.vertices_transformed as f32 / unique_vertex_count as f32 };

	return result;
}

const CACHE_SIZE_MAX: usize = 16;
const VALENCE_MAX: usize = 8;

struct VertexScoreTable {
	cache: [f32; 1 + CACHE_SIZE_MAX],
	live: [f32; 1 + VALENCE_MAX],
}

// Tuned to minimize the ACMR of a GPU that has a cache profile similar to NVidia and AMD
const VERTEX_SCORE_TABLE: VertexScoreTable = VertexScoreTable {
    cache: [0.0, 0.779, 0.791, 0.789, 0.981, 0.843, 0.726, 0.847, 0.882, 0.867, 0.799, 0.642, 0.613, 0.600, 0.568, 0.372, 0.234],
    live: [0.0, 0.995, 0.713, 0.450, 0.404, 0.059, 0.005, 0.147, 0.006],
};

// Tuned to minimize the encoded index buffer size
const VERTEX_SCORE_TABLE_STRIP: VertexScoreTable = VertexScoreTable {
    cache: [0.0, 1.000, 1.000, 1.000, 0.453, 0.561, 0.490, 0.459, 0.179, 0.526, 0.000, 0.227, 0.184, 0.490, 0.112, 0.050, 0.131],
    live: [0.0, 0.956, 0.786, 0.577, 0.558, 0.618, 0.549, 0.499, 0.489],
};

#[derive(Default)]
struct TriangleAdjacency {
	counts: Vec<u32>,
	offsets: Vec<u32>,
	data: Vec<u32>,
}

fn build_triangle_adjacency(adjacency: &mut TriangleAdjacency, indices: &[u32], vertex_count: usize) {
	let face_count = indices.len() / 3;

	// allocate arrays
	adjacency.counts = vec![0; vertex_count];
	adjacency.offsets = vec![0; vertex_count];
	adjacency.data = vec![0; indices.len()];

	// fill triangle counts
	fill_slice(&mut adjacency.counts, 0);

	for index in indices {
		let index = *index as usize;

		assert!(index < vertex_count);

		adjacency.counts[index] += 1;
	}

	// fill offset table
	let mut offset = 0;

	for i in 0..vertex_count {
		adjacency.offsets[i] = offset;
		offset += adjacency.counts[i];
	}

	assert!(offset as usize == indices.len());

	// fill triangle data
	for i in 0..face_count {
		for j in 0..3 {
			let a = indices[i * 3 + j] as usize;
			let o = &mut adjacency.offsets[a];
			adjacency.data[*o as usize] = i as u32;
			*o += 1;
		}
	}

	// fix offsets that have been disturbed by the previous pass
	for i in 0..vertex_count {
		assert!(adjacency.offsets[i] >= adjacency.counts[i]);

		adjacency.offsets[i] -= adjacency.counts[i];
	}
}

fn get_next_vertex_dead_end(dead_end: &[u32], dead_end_top: &mut usize, input_cursor: &mut usize, live_triangles: &[u32], vertex_count: usize) -> u32 {
	// check dead-end stack
	while *dead_end_top != 0 {
		*dead_end_top -= 1;
		let vertex = dead_end[*dead_end_top];

		if live_triangles[vertex as usize] > 0 {
			return vertex;
		}
	}

	// input order
	while *input_cursor < vertex_count {
		if live_triangles[*input_cursor] > 0 {
			return *input_cursor as u32;
		}

		*input_cursor += 1;
	}

	u32::MAX
}

fn get_next_vertex_neighbour(next_candidates: &[u32], live_triangles: &[u32], cache_timestamps: &[u32], timestamp: u32, cache_size: u32) -> u32 {
	let mut best_candidate = u32::MAX;
	let mut best_priority = -1;

	for vertex in next_candidates {
		let vertex = *vertex as usize;

		// otherwise we don't need to process it
		if live_triangles[vertex] > 0 {
			let mut priority: i32 = 0;

			// will it be in cache after fanning?
			if 2 * live_triangles[vertex] + timestamp - cache_timestamps[vertex] <= cache_size {
				priority = timestamp as i32 - cache_timestamps[vertex] as i32; // position in cache
			}

			if priority > best_priority {
				best_candidate = vertex as u32;
				best_priority = priority;
			}
		}
	}

	best_candidate
}

fn vertex_score(table: &VertexScoreTable, cache_position: i32, live_triangles: usize) -> f32 {
	assert!(cache_position >= -1 && cache_position < CACHE_SIZE_MAX as i32);

	let live_triangles_clamped = if live_triangles < VALENCE_MAX { live_triangles } else { VALENCE_MAX };

	table.cache[(1 + cache_position) as usize] + table.live[live_triangles_clamped]
}

fn get_next_triangle_dead_end(input_cursor: &mut usize, emitted_flags: &[bool], face_count: usize) -> u32 {
	// input order
	while *input_cursor < face_count {
		if !emitted_flags[*input_cursor] {
			return *input_cursor as u32;
		}

		*input_cursor += 1;
	}

	u32::MAX
}

fn optimize_vertex_cache_table(destination: &mut [u32], indices: &[u32], vertex_count: usize, table: &VertexScoreTable) {
	assert!(indices.len() % 3 == 0);

	// guard for empty meshes
	if indices.len() == 0 || vertex_count == 0 {
		return;
	}

	let cache_size = 16;
	assert!(cache_size <= CACHE_SIZE_MAX);

	let face_count = indices.len() / 3;

	// build adjacency information
	let mut adjacency = TriangleAdjacency::default();
	build_triangle_adjacency(&mut adjacency, indices, vertex_count);

	// live triangle counts
	let mut live_triangles = adjacency.counts.clone();

	// emitted flags
	let mut emitted_flags = vec![false; face_count];

	// compute initial vertex scores
	let mut vertex_scores = vec![0.0; vertex_count];

	for i in 0..vertex_count {
		vertex_scores[i] = vertex_score(table, -1, live_triangles[i] as usize);
	}

	// compute triangle scores
	let mut triangle_scores = vec![0.0; face_count];

	for i in 0..face_count {
		triangle_scores[i] = indices[i*3..i*3+3].iter().map(|idx| vertex_scores[*idx as usize]).sum();
	}

	let mut cache_holder = [0; 2 * (CACHE_SIZE_MAX + 3)];
	let (mut cache, mut cache_new) = cache_holder.split_at_mut(CACHE_SIZE_MAX + 3);
	let mut cache_count = 0;

	let mut current_triangle = 0;
	let mut input_cursor: usize = 1;

	let mut output_triangle = 0;

	while current_triangle != u32::MAX {
		assert!(output_triangle < face_count);

		let abc_begin = current_triangle as usize * 3;
		let abc = &indices[abc_begin..abc_begin+3];

		// output indices
		destination[output_triangle*3..output_triangle*3+3].copy_from_slice(abc);
		output_triangle += 1;

		// update emitted flags
		emitted_flags[current_triangle as usize] = true;
		triangle_scores[current_triangle as usize] = 0.0;

		// new triangle
		let mut cache_write = 0;
		for e in abc {
			cache_new[cache_write] = *e;
			cache_write += 1;
		}

		// old triangles
		for index in &cache[0..cache_count] {
			if abc.iter().all(|e| *e != *index) {
				cache_new[cache_write] = *index;
				cache_write += 1;
			}
		}

		std::mem::swap(&mut cache, &mut cache_new);
		cache_count = if cache_write > cache_size { cache_size } else { cache_write };

		// update live triangle counts
		for e in abc {
			live_triangles[*e as usize] -= 1;
		}

		// remove emitted triangle from adjacency data
		// this makes sure that we spend less time traversing these lists on subsequent iterations
		for k in 0..3 {
			let index = indices[current_triangle as usize * 3 + k] as usize;

			let neighbours = &mut adjacency.data[adjacency.offsets[index] as usize..];
			let neighbours_size = adjacency.counts[index] as usize;

			for i in 0..neighbours_size {
				let tri = neighbours[i];

				if tri == current_triangle {
					neighbours[i] = neighbours[neighbours_size - 1];
					adjacency.counts[index] -= 1;
					break;
				}
			}
		}

		let mut best_triangle = u32::MAX;
		let mut best_score = 0.0;

		// update cache positions, vertex scores and triangle scores, and find next best triangle
		for i in 0..cache_write {
			let index = cache[i] as usize;

			let cache_position = if i >= cache_size { -1 } else { i as i32 };

			// update vertex score
			let score = vertex_score(table, cache_position, live_triangles[index] as usize);
			let score_diff = score - vertex_scores[index];

			vertex_scores[index] = score;

			// update scores of vertex triangles
			let off = adjacency.offsets[index] as usize;
			let neighbours = &adjacency.data[off..off+adjacency.counts[index] as usize];

			for tri in neighbours {
				assert!(!emitted_flags[*tri as usize]);

				let tri_score = triangle_scores[*tri as usize] + score_diff;
				assert!(tri_score > 0.0);

				if best_score < tri_score {
					best_triangle = *tri;
					best_score = tri_score;
				}

				triangle_scores[*tri as usize] = tri_score;
			}
		}

		// step through input triangles in order if we hit a dead-end
		current_triangle = best_triangle;

		if current_triangle == u32::MAX { 
			current_triangle = get_next_triangle_dead_end(&mut input_cursor, &emitted_flags, face_count);
		}
	}

	assert!(input_cursor == face_count);
	assert!(output_triangle == face_count);
}

/// Reorders indices to reduce the number of GPU vertex shader invocations.
///
/// If index buffer contains multiple ranges for multiple draw calls, this functions needs to be called on each range individually.
///
/// # Arguments
///
/// * `destination`: must contain enough space for the resulting index buffer (`indices.len()` elements)
pub fn optimize_vertex_cache(destination: &mut [u32], indices: &[u32], vertex_count: usize) {
	optimize_vertex_cache_table(destination, indices, vertex_count, &VERTEX_SCORE_TABLE);
}

/// Reorders indices to reduce the number of GPU vertex shader invocations (for strip-like caches).
///
/// Produces inferior results to [optimize_vertex_cache] from the GPU vertex cache perspective.
/// However, the resulting index order is more optimal if the goal is to reduce the triangle strip length or improve compression efficiency.
///
/// # Arguments
///
/// * `destination`: must contain enough space for the resulting index buffer (`indices.len()` elements)
pub fn optimize_vertex_cache_strip(destination: &mut [u32], indices: &[u32], vertex_count: usize) {
	optimize_vertex_cache_table(destination, indices, vertex_count, &VERTEX_SCORE_TABLE_STRIP);
}

/// Reorders indices to reduce the number of GPU vertex shader invocations (for FIFO caches).
///
/// Generally takes ~3x less time to optimize meshes but produces inferior results compared to [optimize_vertex_cache].
/// If index buffer contains multiple ranges for multiple draw calls, this functions needs to be called on each range individually.
///
/// # Arguments
///
/// * `destination`: must contain enough space for the resulting index buffer (`indices.len()` elements)
/// * `cache_size`: should be less than the actual GPU cache size to avoid cache thrashing
pub fn optimize_vertex_cache_fifo(destination: &mut [u32], indices: &[u32], vertex_count: usize, cache_size: u32) {
	assert!(indices.len() % 3 == 0);
	assert!(cache_size >= 3);

	// guard for empty meshes
	if indices.len() == 0 || vertex_count == 0 {
		return;
	}

	let face_count = indices.len() / 3;

	// build adjacency information
	let mut adjacency = TriangleAdjacency::default();
	build_triangle_adjacency(&mut adjacency, indices, vertex_count);

	// live triangle counts
	let mut live_triangles = adjacency.counts.clone();

	// cache time stamps
	let mut cache_timestamps = vec![0; vertex_count];

	// dead-end stack
	let mut dead_end = vec![0; indices.len()];
	let mut dead_end_top = 0;

	// emitted flags
	let mut emitted_flags = vec![false; face_count];

	let mut current_vertex = 0;

	let mut timestamp = cache_size as u32 + 1;
	let mut input_cursor = 1; // vertex to restart from in case of dead-end

	let mut output_triangle = 0;

	while current_vertex != u32::MAX {
		let next_candidates_begin = dead_end_top;

		// emit all vertex neighbours
		let o = adjacency.offsets[current_vertex as usize] as usize;
		let c = adjacency.counts[current_vertex as usize] as usize;
		let neighbours = &adjacency.data[o..o+c];

		for triangle in neighbours {
			let triangle = *triangle as usize;

			if !emitted_flags[triangle] {
				let a = indices[triangle * 3 + 0];
				let b = indices[triangle * 3 + 1];
				let c = indices[triangle * 3 + 2];

				// output indices
				destination[output_triangle * 3 + 0] = a;
				destination[output_triangle * 3 + 1] = b;
				destination[output_triangle * 3 + 2] = c;
				output_triangle += 1;

				// update dead-end stack
				dead_end[dead_end_top + 0] = a;
				dead_end[dead_end_top + 1] = b;
				dead_end[dead_end_top + 2] = c;
				dead_end_top += 3;

				let abc = [a as usize, b as usize, c as usize];

				for i in &abc {
					// update live triangle counts
					live_triangles[*i] -= 1;

					// update cache info
					// if vertex is not in cache, put it in cache
					if timestamp - cache_timestamps[*i] > cache_size {
						cache_timestamps[*i] = timestamp;
						timestamp += 1;
					}
				}

				// update emitted flags
				emitted_flags[triangle] = true;
			}
		}

		// next candidates are the ones we pushed to dead-end stack just now
		let next_candidates = &dead_end[next_candidates_begin..dead_end_top];

		// get next vertex
		current_vertex = get_next_vertex_neighbour(next_candidates, &live_triangles, &cache_timestamps, timestamp, cache_size);

		if current_vertex == u32::MAX {
			current_vertex = get_next_vertex_dead_end(&dead_end, &mut dead_end_top, &mut input_cursor, &live_triangles, vertex_count);
		}
	}

	assert!(output_triangle == face_count);
}

#[cfg(test)]
mod test {
	use super::*;

	#[test]
	fn test_empty() {
		optimize_vertex_cache(&mut [], &[], 0);
		optimize_vertex_cache_fifo(&mut [], &[], 0, 16);
	}
}
