use std::cmp;
use std::collections::hash_map::HashMap;
use std::time::Instant;

use image::GrayImage;
use log::{debug, info};

#[derive(Copy, Clone, Debug)]
pub struct RegionOfInterest {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl RegionOfInterest {
    fn is_corner(&self, x: u32, y: u32) -> bool {
        (x == self.x || x == self.x + self.width - 1) &&
            (y == self.y || y == self.y + self.height - 1)
    }
}

// An iterator over the pixels of a region of interest. Yields pixels in raster
// scan order. An empty ROI (zero width and/or height) yields no pixels.
struct EnumeratePixels<'a> {
    image: &'a GrayImage,
    roi: &'a RegionOfInterest,
    include_interior: bool,

    cur_x: u32,
    cur_y: u32,
}

impl<'a> EnumeratePixels<'a> {
    fn new(image: &'a GrayImage, roi: &'a RegionOfInterest, include_interior: bool)
           -> EnumeratePixels<'a> {
        let (width, height) = image.dimensions();
        assert!(roi.x + roi.width < width);
        assert!(roi.y + roi.height < height);
        EnumeratePixels{image, roi, include_interior, cur_x: roi.x, cur_y: roi.y}
    }
}

impl<'a> Iterator for EnumeratePixels<'a> {
    type Item = (u32, u32, u8);  // x, y, pixel value.

    fn next(&mut self) -> Option<Self::Item> {
        if self.roi.width == 0 || self.roi.height == 0 {
            return None;
        }
        if self.cur_y > self.roi.y + self.roi.height - 1 {
            return None;
        }
        let item:Self::Item = (self.cur_x, self.cur_y,
                               self.image.get_pixel(self.cur_x, self.cur_y).0[0]);
        if self.cur_x == self.roi.x + self.roi.width - 1 {
            self.cur_x = self.roi.x;
            self.cur_y += 1;
        } else {
            let do_all_in_row = self.include_interior ||
                self.cur_y == self.roi.y ||
                self.cur_y == self.roi.y + self.roi.height - 1;
            if do_all_in_row {
                self.cur_x += 1;
            } else {
                // Exclude interior.
                assert!(self.cur_x == self.roi.x);
                self.cur_x = self.roi.x + self.roi.width - 1;
            }
        }
        Some(item)
    }
}

// Returns (mean, stddev) for the given image region. Excludes the brightest
// 5% of pixels.
fn stats_for_roi(image: &GrayImage, roi: &RegionOfInterest)
                 -> (f32, f32) {
    let mut histogram: [u32; 256] = [0; 256];
    for (_x, _y, pixel_value) in EnumeratePixels::new(
        image, roi, /*include_interior=*/true) {
        histogram[pixel_value as usize] += 1;
    }
    debug!("Original histogram: {:?}", histogram);
    // Discard the top 5% of the histogram. We want only background pixels
    // to contribute to the noise estimate.
    let keep_count = (roi.width * roi.height * 95 / 100) as u32;
    let mut kept_so_far = 0;
    let mut first_moment = 0;
    for h in 0..256 {
        let bin_count = histogram[h];
        if kept_so_far + bin_count > keep_count {
            histogram[h] = keep_count - kept_so_far;
        }
        kept_so_far += histogram[h];
        first_moment += histogram[h] * h as u32;
    }
    debug!("De-starred histogram: {:?}", histogram);
    let mean = first_moment as f32 / keep_count as f32;
    let mut second_moment: f32 = 0.0;
    for h in 0..256 {
        second_moment += histogram[h] as f32 * (h as f32 - mean) * (h as f32 - mean);
    }
    let stddev = (second_moment / keep_count as f32).sqrt();
    (mean, stddev)
}

// Estimates the RMS noise of the given image. A small portion of the image
// is processed as follows:
// 1. The 5% brightest pixels are excluded.
// 2. The mean of the N remaining pixels is computed, and the standard
//    deviation is computed in the usuakl way as
//      sqrt(sum((pixel-mean)*(pixel-mean))/N)
//
// To guard against accidentally sampling a bright part of the image (moon?
// streetlamp?), we sample a few image regions to find the darkest one.
fn estimate_noise_from_image(image: &GrayImage) -> f32 {
    let noise_start = Instant::now();
    let (width, height) = image.dimensions();
    let box_size = cmp::min(30, cmp::min(width, height) / 4);
    let mut stats_vec = Vec::<(f32, f32)>::new();
    stats_vec.push(stats_for_roi(image, &RegionOfInterest{
        x: width*1/4 - box_size/2, y: height/2 - box_size/2,
        width: box_size, height: box_size}));
    stats_vec.push(stats_for_roi(image, &RegionOfInterest{
        x: width*2/4 - box_size/2, y: height/2 - box_size/2,
        width: box_size, height: box_size}));
    stats_vec.push(stats_for_roi(image, &RegionOfInterest{
        x: width*3/4 - box_size/2, y: height/2 - box_size/2,
        width: box_size, height: box_size}));
    // Pick the darkest box.
    stats_vec.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let stddev = stats_vec[0].1;
    debug!("Noise estimate {} found in {:?}", stddev, noise_start.elapsed());
    stddev
}

// TODO: doc
// Disucssion:
// TODO. hot performance path
// Returns:
//   0: pixel value to use for possible ROI processing. The value is not
//      background subtracted, but care is taken to not return a hot pixel.
//   1: whether the gate's center pixel satisfies all of the 1d criteria
//      to be further evaluated as a star.
//   2: whether the gate's center pixel is a hot pixel.
fn gate_star_1d(gate: &[u8], noise_estimate: f32, sigma: f32)
                -> (u8, bool, bool) {
    let lb = gate[0] as i16;  // Left border.
    let lm = gate[1] as i16;  // Left margin.
    let l = gate[2] as i16;   // Left neighbor.
    let c = gate[3] as i16;   // Center.
    let r = gate[4] as i16;   // Right neighbor.
    let rm = gate[5] as i16;  // Right margin.
    let rb = gate[6] as i16;  // Right border.
    let c8 = gate[3];

    // Center pixel must be sigma * estimated noise brighter than the
    // estimated background. Do this test first, because it eliminates
    // the vast majority of pixels.
    let sigma_noise: f32 = sigma * noise_estimate;
    let est_background_2 = lb + rb;
    let center_over_background_2 = c + c - est_background_2;
    if center_over_background_2 < (sigma_noise * 2_f32) as i16 {
        return (c8, false, false);
    }

    // Center pixel must be at least as bright as its immediate left/right
    // neighbors.
    if l > c || c < r {
        return (c8, false, false);
    }
    // Center pixel must be strictly brighter than its left/right margin.
    if lm >= c || c <= rm {
        return (c8, false, false);
    }
    // Center pixel must be strictly brighter than the borders.
    if lb >= c || c <= rb {
        return (c8, false, false);
    }
    if l == c {
        // Break tie between left and center.
        if lm > r {
            // Left will have been the center of its own candidate entry.
            return (c8, false, false);
        }
    }
    if c == r {
        // Break tie between center and right.
        if l <= rm {
            // Right will be the center of its own candidate entry.
            return (c8, false, false);
        }
    }
    // Average of l+r must be 0.25 * sigma * estimated noise brighter
    // than the estimated background.
    // Discussion: TODO.
    let sum_neighbors_over_background = l + r - est_background_2;
    if sum_neighbors_over_background < (0.5 * sigma_noise) as i16
    {
        // For ROI processing purposes, replace the hot pixel with its
        // neighbors' value.
        return (((l + r) / 2) as u8, false, /*hot_pixel=*/true);
    }
    // We require the border pixels to be ~uniformly dark. See if there
    // is too much brightness difference between the border pixels.
    // Discussion: TODO.
    let border_diff = (lb - rb).abs();
    if border_diff > (0.5 * sigma_noise) as i16 {
        return (c8, false, false);
    }
    // We have a candidate star from our 1d analysis!
    return (c8, /*is_star_candidate=*/true, false);
}

#[derive(Copy, Clone, Debug)]
struct CandidateFromRowGate {
    x: i32,
    y: i32,
}

// The candidates are returned in raster scan order.
// Returns:
// Vec<CandidateFromRowGate>: the identifed star candidates
// u32: count of hot pixels detected
// Discussion:
// TODO.
fn scan_image_for_candidates(image: &GrayImage, noise_estimate: f32, sigma: f32)
                       -> (Vec<CandidateFromRowGate>, u32)
{
    let row_scan_start = Instant::now();
    let mut hot_pixel_count = 0_u32;
    let (width, height) = image.dimensions();
    let image_pixels: &Vec<u8> = image.as_raw();
    let mut candidates = Vec::<CandidateFromRowGate>::new();
    // We'll generally have way fewer than 1 candidate per row.
    candidates.reserve(height as usize);
    for rownum in 0..height {
        // Get the slice of image_pixels corresponding to this row.
        let row_pixels: &[u8] = &image_pixels.as_slice()
            [(rownum * width) as usize .. ((rownum+1) * width) as usize];
        // Slide a 7 pixel gate across the row.
        let mut center_x = 2_u32;
        for gate in row_pixels.windows(7) {
            center_x += 1;
            let (_pixel_value, is_star_candidate, is_hot_pixel) =
                gate_star_1d(gate, noise_estimate, sigma);
            if is_star_candidate {
                debug!("Candidate at row {} col {}; gate {:?}",
                       rownum, center_x, gate);
                candidates.push(CandidateFromRowGate{x: center_x as i32,
                                                     y: rownum as i32});
            }
            if is_hot_pixel {
                hot_pixel_count += 1;
            }
        }
    }
    info!("Image scan found {} candidates and {} hot pixels in {:?}",
          candidates.len(), hot_pixel_count, row_scan_start.elapsed());
    (candidates, hot_pixel_count)
}

#[derive(Debug)]
struct Blob {
    candidates: Vec<CandidateFromRowGate>,

    // If candidates is empty, that means this blob has been merged into
    // another blob.
    recipient_blob: i32,
}

#[derive(Copy, Clone)]
struct LabeledCandidate {
    candidate: CandidateFromRowGate,
    blob_id: i32,
}

// Discussion: TODO.
fn form_blobs_from_candidates(candidates: Vec<CandidateFromRowGate>, height: i32)
                              -> Vec<Blob> {
    let blobs_start = Instant::now();
    let mut labeled_candidates_by_row = Vec::<Vec<LabeledCandidate>>::new();
    labeled_candidates_by_row.resize(height as usize, Vec::<LabeledCandidate>::new());

    let mut blobs: HashMap<i32, Blob> = HashMap::new();
    let mut next_blob_id = 0;
    // Create an initial singular blob for each candidate.
    for candidate in candidates {
        blobs.insert(next_blob_id, Blob{candidates: vec![candidate],
                                        recipient_blob: -1});
        labeled_candidates_by_row[candidate.y as usize].push(
            LabeledCandidate{candidate, blob_id: next_blob_id});
        next_blob_id += 1;
    }

    // Merge adjacent blobs. Within a row blobs are not adjacent (by definition of
    // how row scanning identifies candidates), so we just need to look for vertical
    // adjacencies.
    // Start processing at row 1 so we can look to previous row for blob merges.
    for rownum in 1..height as usize {
        for rc in &labeled_candidates_by_row[rownum] {
            let rc_pos = rc.candidate.x;
            // See if rc is adjacent to any candidates in the previous row.
            // This is fast since rows usually have very few candidates.
            for prev_row_rc in &labeled_candidates_by_row[rownum - 1] {
                let prev_row_rc_pos = prev_row_rc.candidate.x;
                if prev_row_rc_pos < rc_pos - 3 {
                    continue;
                }
                if prev_row_rc_pos > rc_pos + 3 {
                    break;
                }
                // Adjacent to a candidate in the previous row. Absorb the previous
                // row blob's candidates.
                let recipient_blob_id = rc.blob_id;
                let mut donor_blob_id = prev_row_rc.blob_id;
                let mut donated_candidates: Vec<CandidateFromRowGate>;
                loop {
                    let donor_blob = blobs.get_mut(&donor_blob_id).unwrap();
                    if !donor_blob.candidates.is_empty() {
                        donated_candidates = donor_blob.candidates.drain(..).collect();
                        assert!(donor_blob.recipient_blob == -1);
                        donor_blob.recipient_blob = recipient_blob_id;
                        break;
                    }
                    // This blob got merged to another blob.
                    assert!(donor_blob.recipient_blob != -1);
                    donor_blob_id = donor_blob.recipient_blob;
                }
                let recipient_blob = &mut blobs.get_mut(&recipient_blob_id).unwrap();
                recipient_blob.candidates.append(&mut donated_candidates);
            }
        }
    }
    // Return non-empty blobs. Note that the blob merging we just did will leave
    // some empty entries in the `blobs` mapping.
    let mut non_empty_blobs = Vec::<Blob>::new();
    for (_id, blob) in blobs {
        if !blob.candidates.is_empty() {
            debug!("got blob {:?}", blob);
            non_empty_blobs.push(blob);
        }
    }
    info!("Found {} blobs in {:?}", non_empty_blobs.len(), blobs_start.elapsed());
    non_empty_blobs
}

#[derive(Debug)]
pub struct StarDescription {
    // Location of star centroid in image coordinates. (0.5, 0.5) corresponds
    // to the center of the image's upper left pixel.
    pub centroid_x: f32,
    pub centroid_y: f32,

    // Characterizes the extent or spread of the star in each direction, in
    // pixel size units.
    pub stddev_x: f32,
    pub stddev_y: f32,

    // Mean of the u8 pixel values of the star's region (core plus immediate
    // neighbors). The estimated background is subtracted.
    pub mean_brightness: f32,

    // Count of saturated pixel values.
    pub num_saturated: i32,
}

// TODO: doc.
fn gate_star_2d(blob: &Blob, image: &GrayImage,
                noise_estimate: f32, sigma: f32,
                max_width: u32, max_height: u32) -> Option<StarDescription> {
    let (image_width, image_height) = image.dimensions();
    // Compute the bounding box of all of the blob's center coords.
    let mut x_min = u32::MAX;
    let mut x_max = 0_u32;
    let mut y_min = u32::MAX;
    let mut y_max = 0_u32;
    for candidate in &blob.candidates {
        x_min = cmp::min(x_min, candidate.x as u32);
        x_max = cmp::max(x_max, candidate.x as u32);
        y_min = cmp::min(y_min, candidate.y as u32);
        y_max = cmp::max(y_max, candidate.y as u32);
    }
    let core_x_min = x_min as i32;
    let core_x_max = x_max as i32;
    let core_y_min = y_min as i32;
    let core_y_max = y_max as i32;
    let core_width = (core_x_max - core_x_min) as u32 + 1;
    let core_height = (core_y_max - core_y_min) as u32 + 1;

    let core = RegionOfInterest{
        x: core_x_min as u32, y: core_y_min as u32,
        width: core_width, height: core_height};
    // Reject blob if it is too big.
    if core_width > max_width || core_height > max_height {
        debug!("Blob {:?} too large", core);
        return None;
    }
    // Reject blob if its expansion goes past an image boundary.
    if core_x_min - 3 < 0 || core_x_max + 3 >= image_width as i32 ||
        core_y_min - 3 < 0 || core_y_max + 3 >= image_height as i32
    {
        debug!("Blob {:?} too close to edge", core);
        return None;
    }

    // Expand core bounding box by three pixels in all directions.
    let neighbors = RegionOfInterest{
        x: core_x_min as u32 - 1, y: core_y_min as u32 - 1,
        width: core_width + 2, height: core_height + 2};
    let margin = RegionOfInterest{
        x: core_x_min as u32 - 2, y: core_y_min as u32 - 2,
        width: core_width + 4, height: core_height + 4};
    let perimeter = RegionOfInterest{
        x: core_x_min as u32 - 3, y: core_y_min as u32 - 3,
        width: core_width + 6, height: core_height + 6};

    // Compute average of pixels in core.
    let mut core_sum: i32 = 0;
    let mut core_count: i32 = 0;
    for (_x, _y, pixel_value) in EnumeratePixels::new(
        image, &core, /*include_interior=*/true) {
        core_sum += pixel_value as i32;
        core_count += 1;
    }
    let core_mean = core_sum as f32 / core_count as f32;

    // Compute average of pixels in box immediately surrounding core.
    let mut neighbor_sum: i32 = 0;
    let mut neighbor_count: i32 = 0;
    for (x, y, pixel_value) in EnumeratePixels::new(
        image, &neighbors, /*include_interior=*/false) {
        if neighbors.is_corner(x, y) {
            continue;  // Exclude corner pixels.
        }
        neighbor_sum += pixel_value as i32;
        neighbor_count += 1;
    }
    let neighbor_mean = neighbor_sum as f32 / neighbor_count as f32;
    // Core average must be at least as bright as the neighbor average.
    if core_mean < neighbor_mean {
        debug!("Core average {} is less than neighbor average {} for blob {:?}",
               core_mean, neighbor_mean, core);
        return None;
    }

    // Compute average of pixels in next box out; this is one pixel
    // inward from the outer perimeter.
    let mut margin_sum: i32 = 0;
    let mut margin_count: i32 = 0;
    for (_x, _y, pixel_value) in EnumeratePixels::new(
        image, &margin, /*include_interior=*/false) {
        margin_sum += pixel_value as i32;
        margin_count += 1;
    }
    let margin_mean = margin_sum as f32 / margin_count as f32;
    // Core average must be strictly brighter than the margin average.
    if core_mean <= margin_mean {
        debug!("Core average {} is not greater than margin average {} for blob {:?}",
               core_mean, margin_mean, core);
        return None;
    }

    // Gather information from the outer perimeter. These pixels represent
    // the background.
    let mut perimeter_sum: i32 = 0;
    let mut perimeter_count: i32 = 0;
    let mut perimeter_min = u8::MAX;
    let mut perimeter_max = 0_u8;
    for (_x, _y, pixel_value) in EnumeratePixels::new(
        image, &perimeter, /*include_interior=*/false) {
        perimeter_sum += pixel_value as i32;
        perimeter_count += 1;
        perimeter_min = cmp::min(perimeter_min, pixel_value);
        perimeter_max = cmp::max(perimeter_max, pixel_value);
    }
    let background_est = perimeter_sum as f32 / perimeter_count as f32;
    debug!("background: {} for blob {:?}", background_est, core);

    // We require the perimeter pixels to be ~uniformly dark. See if any
    // perimeter pixel is too bright compared to the darkest perimeter
    // pixel.
    if (perimeter_max - perimeter_min) as f32 > sigma * noise_estimate {
        debug!("Perimeter too varied for blob {:?}", core);
        return None;
    }

    // Verify that core average exceeds background by sigma * noise.
    if core_mean - background_est < sigma * noise_estimate {
        debug!("Core too weak for blob {:?}", core);
        return None;
    }
    // Verify that the neighbor average exceeds background by
    // 0.25 * sigma * noise.
    if neighbor_mean - background_est < 0.25 * sigma * noise_estimate {
        debug!("Neighbors too weak for blob {:?}", core);
        return None;
    }

    // Star passes all of the 2d gates!

    // Process the interior pixels (core plus immediate neighbors) to
    // obtain moments. Also note the count of saturated pixels.
    let mut num_saturated = 0;
    let mut m0: f32 = 0.0;
    let mut m1x: f32 = 0.0;
    let mut m1y: f32 = 0.0;
    for (x, y, pixel_value) in EnumeratePixels::new(
        image, &neighbors, /*include_interior=*/true) {
        if pixel_value == 255 {
            num_saturated += 1;
        }
        let val_minus_bkg = pixel_value as f32 - background_est;
        m0 += val_minus_bkg;
        m1x += x as f32 * val_minus_bkg;
        m1y += y as f32 * val_minus_bkg;
    }
    // We use simple center-of-mass as the centroid.
    let centroid_x = m1x / m0;
    let centroid_y = m1y / m0;
    // Compute second moment about the centroid.
    let mut m2x_c: f32 = 0.0;
    let mut m2y_c: f32 = 0.0;
    for (x, y, pixel_value) in EnumeratePixels::new(
        image, &neighbors, /*include_interior=*/true) {
        let val_minus_bkg = pixel_value as f32 - background_est;
        m2x_c += (x as f32 - centroid_x) * (x as f32 - centroid_x) * val_minus_bkg;
        m2y_c += (y as f32 - centroid_y) * (y as f32 - centroid_y) * val_minus_bkg;
    }
    let variance_x = m2x_c / m0;
    let variance_y = m2y_c / m0;
    Some(StarDescription{centroid_x: (centroid_x + 0.5) as f32,
                         centroid_y: (centroid_y + 0.5) as f32,
                         stddev_x: variance_x.sqrt() as f32,
                         stddev_y: variance_y.sqrt() as f32,
                         mean_brightness:
                         m0 / (neighbors.width * neighbors.height) as f32,
                         num_saturated})
}

// TODO: ROI
// TODO: exhaustive_2d.
// TODO: return roi summary, hot pixel count, noise estimate
// TODO: doc.
pub fn get_stars_from_image(image: &GrayImage, sigma: f32,
                            return_candidates: bool)
                            -> (Vec<StarDescription>, u32) {
    let mut noise_estimate = estimate_noise_from_image(image);
    if noise_estimate < 1.0 {
        // Likely the image background is crushed to black.
        noise_estimate = 1.0;
    }
    let (candidates, hot_pixel_count) =
        scan_image_for_candidates(image, noise_estimate, sigma);
    let mut stars = Vec::<StarDescription>::new();
    if return_candidates {
        // Debugging feature.
        for candidate in candidates {
            stars.push(StarDescription{
                centroid_x: candidate.x as f32,
                centroid_y: candidate.y as f32,
                stddev_x: 0_f32,
                stddev_y: 0_f32,
                mean_brightness: 0_f32,
                num_saturated: 0});
        }
        return (stars, hot_pixel_count);
    }
    let blobs = form_blobs_from_candidates(candidates, image.height() as i32);
    let get_stars_start = Instant::now();
    for blob in blobs {
        match gate_star_2d(&blob, image, noise_estimate, sigma, 5, 5) {
            Some(x) => stars.push(x),
            None => ()
        }
    }
    debug!("2d star gating found {} stars in {:?}",
          stars.len(), get_stars_start.elapsed());
    (stars, hot_pixel_count)
}

// The information here is from original pixel data (not background subtracted)
// but with hot pixels replaced with interpolated neighbor values.
#[derive(Debug)]
#[allow(dead_code)]
pub struct RegionOfInterestSummary {
    // Histogram of pixel values in the ROI.
    pub histogram: [u32; 256],

    // Each element is the mean of a row of the ROI. Size is thus the ROI height.
    pub horizontal_projection: Vec<f32>,

    // Each element is the mean of a column of the ROI. Size is thus the ROI
    // width.
    pub vertical_projection: Vec<f32>,
}

    // let roi_size = cmp::min(width, height) / 3;
    // let roi = RegionOfInterest{x: (width - roi_size) / 2,
    //                            y: (height - roi_size) / 2,
    //                            width: roi_size, height: roi_size};
    // While looking for star candidates, compute extra information for a
    // moderate-sized square ROI in the center of the image.
    // Discussion: TODO (center is likely to avoid bright interference during
    // initial focus and centering interactions).
// Gathers information the region of interest. The pixel data feeding this
// information is not background subtracted, but hot pixels are replaced with
// interpolated neighbor values.
pub fn summarize_region_of_interest(image: &GrayImage, roi: &RegionOfInterest,
                                    noise_estimate: f32, sigma: f32)
                                    -> RegionOfInterestSummary {
    let process_roi_start = Instant::now();

    let (width, height) = image.dimensions();
    assert!(roi.y + roi.height < height);
    // Sliding gate needs to extend past left and right edges of ROI. Make sure
    // there's enough image.
    let gate_leftmost: i32 = roi.x as i32 - 3;
    let gate_rightmost = roi.x + roi.width + 3;  // One past.
    assert!(gate_leftmost >= 0);
    assert!(gate_rightmost <= width);
    let image_pixels: &Vec<u8> = image.as_raw();

    let mut histogram: [u32; 256] = [0_u32; 256];
    let mut horizontal_projection_sum = Vec::<u32>::new();
    let mut vertical_projection_sum = Vec::<u32>::new();
    horizontal_projection_sum.resize(roi.height as usize, 0_u32);
    vertical_projection_sum.resize(roi.width as usize, 0_u32);

    for rownum in roi.y..roi.y + roi.height {
        // Get the slice of image_pixels corresponding to this row of the ROI.
        let row_start = (rownum * width) as usize;
        let row_pixels: &[u8] = &image_pixels.as_slice()
            [row_start + gate_leftmost as usize ..
             row_start + gate_rightmost as usize];
        // Slide a 7 pixel gate across the row.
        let mut center_x = 2_u32;
        for gate in row_pixels.windows(7) {
            center_x += 1;
            let (pixel_value, _is_star_candidate, _is_hot_pixel) =
                gate_star_1d(gate, noise_estimate, sigma);
            histogram[pixel_value as usize] += 1;
            horizontal_projection_sum[(rownum - roi.y) as usize]
                += pixel_value as u32;
            vertical_projection_sum[(center_x - roi.x) as usize]
                += pixel_value as u32;

        }
    }
    let h_proj: Vec<f32> = horizontal_projection_sum.into_iter().map(
        |x| x as f32 / roi.width as f32).collect();
    let v_proj: Vec<f32> = vertical_projection_sum.into_iter().map(
        |x| x as f32 / roi.height as f32).collect();

    debug!("ROI processing completed in {:?}", process_roi_start.elapsed());
    RegionOfInterestSummary{histogram,
                            horizontal_projection: h_proj,
                            vertical_projection: v_proj,
    }
}
