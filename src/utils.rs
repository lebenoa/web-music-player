#[inline]
pub fn without_extension(filename: &str) -> &str {
    filename
        .rfind('.')
        .map(|i| &filename[..i])
        .unwrap_or(filename)
}

/// Height > Width will break this but there's no way right?  
/// Width and height divided by 2 then minus each other to find the offset
#[inline]
pub fn find_offset_to_center(width: u32, height: u32) -> u32 {
    (width / 2) - (height / 2)
}