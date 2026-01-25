use askama::Template;
use super::{html, lang_util};
use crate::error::AppError;
use crate::template::{StoryPart, StoryTemplate};
use crate::types::StoryDownload;
use anyhow::{anyhow, Result};
use base64::prelude::*;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use sanitize_filename::{sanitize_with_options, Options};
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::{
    collections::HashMap,
    io::{Cursor, Read},
    path::Path,
};
use tracing::{info, instrument, warn};
use wp_mini::field::{LanguageField, PartStubField, StoryField, UserStubField};
use wp_mini::types::StoryResponse;
use wp_mini::WattpadClient;
use zip::ZipArchive;

static PLACEHOLDER_IMAGE_DATA: &[u8] = include_bytes!("../assets/placeholder.jpg");

// --- PUBLIC API FUNCTIONS ---

/// Downloads and processes a Wattpad story, saving the result as a single HTML file.
///
/// Excluded for wasm32
///
/// # Arguments
/// * `output_path` - The directory where the final `.html` file will be saved.
///
/// # Returns
/// A `Result` containing the full `PathBuf` to the generated file.
#[cfg(not(target_arch = "wasm32"))]
#[instrument(skip(reqwest_client, wattpad_client, concurrent_requests), fields(id = story_id, path = %output_path.display()))]
pub async fn download_story_to_folder(
    wattpad_client: &WattpadClient,
    reqwest_client: &Client,
    story_id: u64,
    embed_images: bool,
    concurrent_requests: usize,
    output_path: &Path,
    extra_fields: Option<&[StoryField]>,
) -> Result<StoryDownload<PathBuf>> {
    let (html_content, sanitized_title, story_metadata) = prepare_html(
        wattpad_client,
        reqwest_client,
        story_id,
        embed_images,
        concurrent_requests,
        extra_fields,
    )
    .await?;

    let final_path = output_path.join(format!("{}.html", sanitized_title));
    
    std::fs::write(&final_path, html_content)
        .map_err(|e| anyhow!("Failed to write HTML file: {:?}", e))?;

    info!(path = %final_path.display(), "Successfully generated HTML file");
    Ok(StoryDownload {
        sanitized_title,
        epub_response: final_path,
        metadata: story_metadata,
    })
}

/// Downloads and processes a Wattpad story, saving the result to provided file.
///
/// Excluded for wasm32
///
/// # Arguments
/// * `output_file` - The file path of the final `.html` file.
///
/// # Returns
/// A `Result` containing the full `PathBuf` to the generated file.
#[cfg(not(target_arch = "wasm32"))]
#[instrument(skip(reqwest_client, wattpad_client, concurrent_requests), fields(id = story_id, path = %output_file.display()))]
pub async fn download_story_to_file(
    wattpad_client: &WattpadClient,
    reqwest_client: &Client,
    story_id: u64,
    embed_images: bool,
    concurrent_requests: usize,
    output_file: &Path,
    extra_fields: Option<&[StoryField]>,
) -> Result<StoryDownload<PathBuf>> {
    let (html_content, sanitized_title, story_metadata) = prepare_html(
        wattpad_client,
        reqwest_client,
        story_id,
        embed_images,
        concurrent_requests,
        extra_fields,
    )
    .await?;

    std::fs::write(output_file, html_content)
        .map_err(|e| anyhow!("Failed to write HTML file: {:?}", e))?;

    info!(path = %output_file.display(), "Successfully generated HTML file");
    Ok(StoryDownload {
        sanitized_title,
        epub_response: output_file.to_path_buf(),
        metadata: story_metadata,
    })
}

/// Downloads and processes a Wattpad story, returning the HTML as an in-memory string.
///
/// # Returns
/// A `Result` containing the `String` of the generated HTML content.
#[instrument(skip(reqwest_client, wattpad_client, concurrent_requests), fields(id = story_id))]
pub async fn download_story_to_memory(
    wattpad_client: &WattpadClient,
    reqwest_client: &Client,
    story_id: u64,
    embed_images: bool,
    concurrent_requests: usize,
    extra_fields: Option<&[StoryField]>,
) -> Result<StoryDownload<String>> {
    let (html_content, sanitized_title, story_metadata) = prepare_html(
        wattpad_client,
        reqwest_client,
        story_id,
        embed_images,
        concurrent_requests,
        extra_fields,
    )
    .await?;

    info!(
        bytes = html_content.len(),
        "Successfully generated HTML in memory"
    );
    Ok(StoryDownload {
        sanitized_title,
        epub_response: html_content,
        metadata: story_metadata,
    })
}

// --- PRIVATE CORE LOGIC ---

/// Core internal function to fetch, process, and render the Askama template.
/// This function is not concerned with the final output format (file or memory).
/// It returns the rendered HTML string, sanitized title, and metadata.
async fn prepare_html(
    wattpad_client: &WattpadClient,
    reqwest_client: &Client,
    story_id: u64,
    embed_images: bool,
    concurrent_requests: usize,
    extra_fields: Option<&[StoryField]>,
) -> Result<(String, String, StoryResponse)> {
    info!("Starting story download and processing (HTML Mode)");

    // --- 1. Fetch Story Info ---
    let mut story_fields: Vec<StoryField> = vec![
        StoryField::Title,
        StoryField::Description,
        StoryField::Cover,
        StoryField::ModifyDate,
        StoryField::Language(vec![LanguageField::Id]),
        StoryField::User(vec![UserStubField::Username, UserStubField::Avatar]),
        StoryField::Parts(vec![PartStubField::Id, PartStubField::Title]),
    ];

    if let Some(fields) = extra_fields {
        story_fields.extend_from_slice(fields);
    }

    story_fields.sort();
    story_fields.dedup();

    let story = wattpad_client
        .story
        .get_story_info(story_id, Some(&story_fields))
        .await
        .map_err(|_| AppError::MetadataFetchFailed)?;

    info!(title = ?story.title, "Successfully fetched story metadata");

    // --- 2. Fetch Story Content as a ZIP ---
    let zip_bytes = wattpad_client
        .story
        .get_story_content_zip(story_id)
        .await
        .map_err(|_| AppError::DownloadFailed)?;

    info!("Successfully downloaded story content ZIP");

    // --- 3. Process ZIP in Memory ---
    let mut chapter_html_map: HashMap<i64, String> = HashMap::new();
    let zip_cursor = Cursor::new(zip_bytes);
    let mut archive = ZipArchive::new(zip_cursor)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let file_name = match Path::new(file.name()).file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => continue,
        };

        if let Ok(part_id) = file_name.parse::<i64>() {
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            chapter_html_map.insert(part_id, contents);
        }
    }

    // --- 4. Process Chapters Concurrently ---
    let chapter_metadata = story.parts.clone().ok_or(AppError::MetadataFetchFailed)?;
    let total_chapter_count = chapter_metadata.len();
    info!(count = total_chapter_count, "Starting chapter processing");

    // Prepare ordered iterator of data to feed into concurrent stream
    // We map: (Index, Title, RawHTML)
    let ordered_chapters: Vec<(usize, String, String)> = chapter_metadata
        .into_iter()
        .enumerate()
        .filter_map(|(i, part)| {
            let id = part.id? as i64;
            // Retrieve content from map.
            let html = chapter_html_map.remove(&id)?;
            Some((i, part.title.unwrap_or_default(), html))
        })
        .collect();

    // Run processing in parallel using buffer_unordered
    let mut processed_futures = stream::iter(ordered_chapters)
        .map(|(index, title, html)| async move {
            let part_result = process_chapter(
                reqwest_client,
                &title,
                &html,
                embed_images,
                concurrent_requests,
            )
            .await;
            (index, part_result)
        })
        .buffer_unordered(concurrent_requests)
        .collect::<Vec<_>>()
        .await;

    // --- 5. Re-sort and Collect ---
    // buffer_unordered scrambles the order, so we sort by the index we preserved.
    processed_futures.sort_by_key(|(idx, _)| *idx);

    let mut final_parts: Vec<StoryPart> = Vec::new();
    for (_, result) in processed_futures {
        match result {
            Ok(part) => final_parts.push(part),
            Err(e) => warn!("Failed to process a chapter: {}", e),
        }
    }

    info!(
        success_count = final_parts.len(),
        total_count = total_chapter_count,
        "Finished chapter processing"
    );

    // --- 6. Prepare Assets (Cover & Avatar) ---
    // NOTE: These must be Raw Base64 (no "data:" prefix) because the HTML template
    // likely handles the prefix for these specific fields.
    
    let cover_b64 = match story.cover.as_deref() {
        Some(url) => {
            let high_res = url.replace("-256-", "-512-");
            // Discard mime type, keep only base64 string
            download_image_base64(reqwest_client, &high_res).await.map(|(_, b64)| b64).unwrap_or_else(get_placeholder_base64)
        }
        None => get_placeholder_base64(),
    };

    let avatar_b64 = match story.user.as_ref().and_then(|u| u.avatar.as_deref()) {
        Some(url) => download_image_base64(reqwest_client, url).await.map(|(_, b64)| b64).unwrap_or_else(get_placeholder_base64),
        None => get_placeholder_base64(),
    };

    // --- 7. Render Template ---
    let author_name = story.user.as_ref().and_then(|u| u.username.as_deref()).unwrap_or("Unknown");
    let modify_date = story.modify_date.as_deref().unwrap_or("");
    let description = story.description.as_deref().unwrap_or("");
    let story_title = story.title.as_deref().unwrap_or("Untitled");
    
    let language_id = story
        .language
        .as_ref()
        .and_then(|lang| lang.id)
        .unwrap_or(1); // Default to English (1) if missing

    let lang_code = lang_util::get_lang_code(language_id);

    let direction = lang_util::get_direction_for_lang_id(language_id);

    let tmpl = StoryTemplate {
        title: story_title,
        author: author_name,
        published: modify_date,
        description: description,
        cover: &cover_b64,
        avatar: &avatar_b64,
        story_id: &story_id.to_string(),
        lang: lang_code,
        direction: &direction,
        no_parts: final_parts.len(),
        parts: final_parts,
    };

    let rendered_html = tmpl.render().map_err(|e| anyhow!("Template render failed: {}", e))?;

    let sanitized_title = format!(
        "{}-{}",
        story_id,
        sanitize_with_options(
            story_title,
            Options {
                replacement: "_",
                ..Default::default()
            }
        )
    );

    Ok((rendered_html, sanitized_title, story))
}

// --- PRIVATE HELPER FUNCTIONS ---

#[instrument(skip(client, html_in), fields(title))]
async fn process_chapter(
    client: &Client,
    title: &str,
    html_in: &str,
    embed_images: bool,
    concurrent_requests: usize,
) -> Result<StoryPart> {
    let image_map = if embed_images {
        let image_urls = html::collect_image_urls(html_in)?;

        // Download images concurrently
        let downloaded_images = stream::iter(image_urls)
            .map(|url| async move {
                let result = download_image_base64(client, &url).await;
                (url, result)
            })
            .buffer_unordered(concurrent_requests)
            .collect::<Vec<_>>()
            .await;

        let mut map = HashMap::new();
        for (url, result) in downloaded_images {
            let src_replacement = match result {
                Some((mime, b64)) => format!("data:{};base64,{}", mime, b64), // Full Data URI
                None => format!("data:image/jpeg;base64,{}", get_placeholder_base64()), // Placeholder Data URI
            };
            map.insert(url, src_replacement);
        }
        map
    } else {
        HashMap::new()
    };

    // Replace original image URLs with Data URIs in the HTML
    let cleaned_html = html::rewrite_and_clean_html(html_in, embed_images, &image_map)?;

    Ok(StoryPart {
        title: title.to_string(),
        content: cleaned_html,
    })
}

/// Downloads an image and returns a tuple of (MimeType, Base64String).
//async fn download_image_base64(client: &Client, url: &str) -> Option<(String, String)> {
  //  if reqwest::Url::parse(url).is_err() {
    //    return None;
    //}

    //match client.get(url).send().await {
      //  Ok(resp) if resp.status().is_success() => {
        //    let mime = resp.headers()
          //      .get(reqwest::header::CONTENT_TYPE)
            //    .and_then(|v| v.to_str().ok())
              //  .unwrap_or("image/jpeg")
                //.to_string();

      //      if let Ok(bytes) = resp.bytes().await {
        //        let b64 = BASE64_STANDARD.encode(&bytes);
          //      Some((mime, b64))
           // } else {
             //   None
            //}
      //  },
      //  _ => None,
    //}
//}

// [SUPERIOR VERSION] Uses binary magic bytes
async fn download_image_base64(client: &Client, url: &str) -> Option<(String, String)> {
    // 1. Check URL valid (Fast fail)
    if reqwest::Url::parse(url).is_err() {
        return None;
    }

    // 2. Download
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            // 3. Get the raw bytes
            if let Ok(bytes) = resp.bytes().await {
                
                // We use your existing 'infer' function on the actual data.
                // This guarantees the MIME type matches the image data.
                let extension = html::infer_extension_from_data(&bytes).unwrap_or("jpg");
                
                let mime = match extension {
                    "png" => "image/png",
                    "gif" => "image/gif",
                    _ => "image/jpeg", // Fallback for jpg or unknown
                };
                
                // 4. Encode
                let b64 = BASE64_STANDARD.encode(&bytes);
                Some((mime.to_string(), b64))
            } else {
                None
            }
        },
        _ => None,
    }
}

fn get_placeholder_base64() -> String {
    BASE64_STANDARD.encode(PLACEHOLDER_IMAGE_DATA)
}