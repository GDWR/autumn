use crate::config::{get_tag, Config, ServeConfig};
use crate::db::*;
use crate::util::result::Error;
use crate::util::variables::{get_s3_bucket, LOCAL_STORAGE_PATH, USE_S3};

use actix_web::{web::Query, HttpRequest, HttpResponse};
use image::{io::Reader as ImageReader, ImageError};
use mongodb::bson::doc;
use serde::Deserialize;
use std::cmp;
use std::io::Cursor;
use std::path::PathBuf;
use log::error;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

#[derive(Deserialize, Debug)]
pub struct Resize {
    pub size: Option<isize>,
    pub width: Option<isize>,
    pub height: Option<isize>,
    pub max_side: Option<isize>,
}

pub fn try_resize(buf: Vec<u8>, width: u32, height: u32) -> Result<Vec<u8>, ImageError> {
    let mut bytes: Vec<u8> = Vec::new();
    let config = Config::global();

    let image = ImageReader::new(Cursor::new(buf))
        .with_guessed_format()?
        .decode()?
        // resize_exact is about 2.5x slower,
        //  thumb approximation doesn't have terrible quality so it's fine to stick with
        //.resize_exact(width as u32, height as u32, image::imageops::FilterType::Gaussian)
        .thumbnail_exact(width, height);

    match config.serve {
        ServeConfig::PNG => {
            let mut writer = Cursor::new(&mut bytes);
            image.write_to(&mut writer, image::ImageOutputFormat::Png)?;
        }
        ServeConfig::WEBP { quality } => {
            let encoder = webp::Encoder::from_image(&image).expect("Could not create encoder.");
            if let Some(quality) = quality {
                bytes = encoder.encode(quality).to_vec();
            } else {
                bytes = encoder.encode_lossless().to_vec();
            }
        }
    }

    Ok(bytes)
}

pub async fn fetch_file(
    id: &str,
    tag: &str,
    metadata: Metadata,
    resize: Option<Resize>,
) -> Result<(Vec<u8>, Option<String>), Error> {
    let mut contents = vec![];
    let config = Config::global();

    if *USE_S3 {
        let bucket = get_s3_bucket(tag)?;
        let (data, code) = bucket
            .get_object(format!("/{}", id))
            .await
            .map_err(|_| Error::S3Error)?;

        if code != 200 {
            return Err(Error::S3Error);
        }

        contents = data;
    } else {
        let path: PathBuf = format!("{}/{}", *LOCAL_STORAGE_PATH, id)
            .parse()
            .map_err(|_| Error::IOError)?;

        let mut f = File::open(path.clone()).await.map_err(|_| Error::IOError)?;
        f.read_to_end(&mut contents)
            .await
            .map_err(|_| Error::IOError)?;
    }

    // If not an image, we don't perform any further alterations
    let (width, height) = match metadata {
        Metadata::Image { width: w, height: h } => (w, h),
        _ => return Ok((contents, None)),
    };


    if let Some(params) = resize {
        let (new_width, new_height) = match params {

            // ?size=...
            Resize { size: Some(requested_size), .. } => {
                let smallest_size = cmp::min(requested_size, cmp::min(width, height));
                (smallest_size, smallest_size)
            }

            // ?max_side=...
            Resize { max_side: Some(requested_max_side), .. } => {
                if width <= height {
                    let h = cmp::min(height, requested_max_side);
                    ((width as f32 * (h as f32 / height as f32)) as isize, h)
                } else {
                    let w = cmp::min(width, requested_max_side);
                    (w, (height as f32 * (w as f32 / width as f32)) as isize)
                }
            }

            // ?width=...&height=...
            Resize { width: Some(requested_width), height: Some(requested_height), .. } => {
                (cmp::min(width, requested_width), cmp::min(height, requested_height))
            }

            // ?width=...
            Resize { width: Some(requested_width), .. } => {
                let w = cmp::min(width, requested_width);
                (w, (w as f32 * (height as f32 / width as f32)) as isize)
            }

            // ?height=...
            Resize { height: Some(requested_height), .. } => {
                let h = cmp::min(height, requested_height);
                ((h as f32 * (width as f32 / height as f32)) as isize, h)
            }

            _ => return Ok((contents, None)),
        };


        let resize_task = actix_web::web::block(
            move || try_resize(contents, new_width as u32, new_height as u32));

        match resize_task.await.map_err(|_| Error::BlockingError)? {
            Ok(resized_content) => Ok((resized_content, Some(match config.serve {
                ServeConfig::PNG => "image/png",
                ServeConfig::WEBP { .. } => "image/webp",
            }.to_string()))),
            Err(e) => {
                error!("Failed to resize image. id={id} params={params:?} e={e}");
                Err(Error::IOError)
            }
        }
    } else {
        // No alterations requested via query params
        Ok((contents, None))
    }
}

pub async fn get(req: HttpRequest, resize: Query<Resize>) -> Result<HttpResponse, Error> {
    let tag = get_tag(&req)?;

    let id = req.match_info().query("filename");
    let file = find_file(id, tag.clone()).await?;

    if let Some(true) = file.deleted {
        return Err(Error::NotFound);
    }

    let (contents, content_type) = fetch_file(id, &tag.0, file.metadata, Some(resize.0)).await?;
    let content_type = content_type.unwrap_or(file.content_type);

    // This list should match files accepted
    // by upload.rs#L68 as allowed images / videos.
    let diposition = match content_type.as_ref() {
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "video/mp4" | "video/webm"
        | "video/webp" | "audio/quicktime" | "audio/mpeg" => "inline",
        _ => "attachment",
    };

    Ok(HttpResponse::Ok()
        .insert_header(("Content-Disposition", diposition))
        .insert_header(("Cache-Control", crate::CACHE_CONTROL))
        .content_type(content_type)
        .body(contents))
}
