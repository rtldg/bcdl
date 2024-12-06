// SPDX-License-Identifier: MIT
// Copyright 2021 yyyyyyyan <contact@yyyyyyyan.tech>
// Copyright 2024 rtldg <rtldg@protonmail.com>

// https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/bandcamp.py
// https://github.dev/yyyyyyyan/bandcamper

use std::sync::LazyLock;

use anyhow::{anyhow, bail, Context};
use clap::Parser;
use futures_util::StreamExt;
use serde_json_path::JsonPath;
use tokio::io::AsyncWriteExt;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None, flatten_help = true, disable_help_subcommand = true)]
struct Args {
	#[arg(short, long, env = "MUSIC_FOLDER", default_value = "./music")]
	music_folder: String,
	#[arg(short, long, env = "NO_ARTIST_SUBFOLDER")]
	no_artist_subfolder: Option<bool>,
	#[arg(required(true))]
	urls_or_batch_file: Vec<String>,
}

static ARGS: LazyLock<Args> = LazyLock::new(Args::parse);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	if let Err(e) = dotenvy::dotenv() {
		if let dotenvy::Error::LineParse(_, _) = e {
			Err(e)?;
		}
	}

	let mut urls = vec![];

	for arg in &ARGS.urls_or_batch_file {
		if arg.starts_with("http://") || arg.starts_with("https://") {
			urls.push(reqwest::Url::parse(arg)?);
		} else {
			let content = tokio::fs::read_to_string(arg).await?;
			for line in content.lines() {
				let line = line.trim();
				if !line.is_empty() {
					urls.push(reqwest::Url::parse(line)?);
				}
			}
		}
	}

	tokio::fs::create_dir_all(&ARGS.music_folder).await?;

	// println!("{:?}", urls);

	download_urls(&urls, std::path::Path::new(&ARGS.music_folder)).await
}

async fn download_urls(urls: &[reqwest::Url], music_folder: &std::path::Path) -> anyhow::Result<()> {
	let mut items = vec![];
	let mut artists = vec![];

	for url in urls {
		if url.path().starts_with("/album/") || url.path().starts_with("/track/") {
			items.push(url.clone());
		} else if url.path().starts_with("/music") || url.path() == "/" {
			artists.push(url);
		} else {
			return Err(anyhow!("non-bandcamp album or artist page url? url='{url}'"));
		}
	}

	let chrome_user_agent = {
		let resp: Vec<String> = reqwest::Client::new()
			.get("https://jnrbsn.github.io/user-agents/user-agents.json")
			.header(
				"user-agent",
				format!(
					"{}/{} ({})",
					env!("CARGO_PKG_NAME"),
					env!("CARGO_PKG_VERSION"),
					env!("CARGO_PKG_REPOSITORY")
				),
			)
			.send()
			.await?
			.json()
			.await?;
		resp.iter()
			.find(|s| s.contains("Windows") && !s.contains("Edg") && !s.contains("Firefox"))
			.unwrap()
			.clone()
	};

	let client = reqwest::ClientBuilder::new()
		.user_agent(chrome_user_agent)
		.build()
		.unwrap();

	for artist in artists {
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		println!("Checking for items from {}", artist.as_str());

		let html = client.get(artist.clone()).send().await?.text().await?;
		let document = scraper::Html::parse_document(&html);
		static MUSIC_GRID_SELECTOR: LazyLock<scraper::Selector> =
			LazyLock::new(|| scraper::Selector::parse("#music-grid > li > a").unwrap());
		for element in document.select(&MUSIC_GRID_SELECTOR) {
			let mut item_url = artist.clone();
			item_url.set_path(element.attr("href").unwrap());
			println!("  found {}", item_url.as_str());
			items.push(item_url);
		}

		/*
		let music_grid: serde_json::Value = serde_json::from_str(
			document
				.select(&MUSIC_GRID_SELECTOR)
				.next()
				.context("couldn't find pagedata info on page")?
				.attr("data-client-items")
				.context("missing pagedata?")?,
		)?;
		for item in music_grid.as_array().unwrap() {
			let mut item_url = artist.clone();
			item_url.set_path(item["page_url"].as_str().unwrap());
			println!("  found {}", item_url.as_str());
			items.push(item_url);
		}
		// println!("\n{}\n", serde_json::to_string_pretty(&music_grid).unwrap());
		*/
	}

	for item in &items {
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		println!("Checking {}", item.as_str());
		download_item(&client, item, music_folder).await?;
	}

	Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum FreeDownload {
	Page(reqwest::Url),
	RequiresEmail,
}

#[derive(Debug, PartialEq, Eq)]
enum ItemType {
	Album,
	Track,
}

impl ItemType {
	pub fn from_json(json: &serde_json::Value) -> Option<ItemType> {
		let type_ = json.get("@type")?;
		if type_ == "MusicAlbum" {
			Some(ItemType::Album)
		} else if type_ == "MusicRecording" {
			Some(ItemType::Track)
		} else {
			None
		}
	}
}

#[derive(Debug)]
struct ItemInfo {
	item_type: ItemType,

	item_id: i64,

	// price: ItemPrice,
	free_download: Option<FreeDownload>,

	name: String,
	publisher_name: String,
	artist_name: String,
	published: jiff::Timestamp,
}

// TODO: could potentially leave our string as 0 characters...
// Does not filter other things like con/aux/nul/etc because this isn't going to be the entire filename...
fn fucky_sanitize_basename_for_windows(s: &str) -> String {
	const OUR_MAX_LEN: usize = 249; // +5 for a file extension (.zip / .flac) +1 for a little breathing room :innocent:
	let mut s: String = s
		.chars()
		.filter(|c| !c.is_ascii_control() && "\"\\/?<>:*|".find(*c).is_none())
		.take(250)
		.collect();
	// continously pop codepoints off `s` until the utf16 string is less than 250 u16 units...
	loop {
		let utf16 = widestring::Utf16String::from_str(&s);
		// shrimplest way to deal with this...
		// TODO: doesn't handle grapheme clusters & whatnot very well... (read as: doesn't handle them correctly at all)
		if utf16.len() > OUR_MAX_LEN {
			let _ = s.pop();
		} else {
			break;
		}
	}
	s
}

fn get_file_download_path(item_info: &ItemInfo, publisher_folder: &std::path::Path) -> std::path::PathBuf {
	let filename = format!(
		"{} - {} - {}",
		item_info.published.strftime("%Y-%m-%d"),
		item_info.artist_name,
		item_info.name
	);

	let filename = fucky_sanitize_basename_for_windows(&filename)
		+ if item_info.item_type == ItemType::Album {
			".zip"
		} else {
			".flac"
		};

	publisher_folder.join(filename)
}

fn parse_pagedata(document: &scraper::Html) -> anyhow::Result<serde_json::Value> {
	static PAGEDATA_SELECTOR: LazyLock<scraper::Selector> =
		LazyLock::new(|| scraper::Selector::parse("#pagedata").unwrap());
	Ok(serde_json::from_str(
		document
			.select(&PAGEDATA_SELECTOR)
			.next()
			.context("couldn't find pagedata info on page")?
			.attr("data-blob")
			.context("missing pagedata?")?,
	)?)
}

impl ItemInfo {
	pub fn parse(html: &str) -> anyhow::Result<ItemInfo> {
		let document = scraper::Html::parse_document(html);

		static LDJSON_SELECTOR: LazyLock<scraper::Selector> =
			LazyLock::new(|| scraper::Selector::parse(r#"script[type="application/ld+json"]"#).unwrap());
		let ldjson: serde_json::Value = serde_json::from_str(
			document
				.select(&LDJSON_SELECTOR)
				.next()
				.context("couldn't find ld+json album info on page")?
				.text()
				.next()
				.context("album info element is missing text?")?,
		)?;

		static TRALBUM_SELECTOR: LazyLock<scraper::Selector> =
			LazyLock::new(|| scraper::Selector::parse(r#"script[data-tralbum]"#).unwrap());
		let tralbum: serde_json::Value = serde_json::from_str(
			document
				.select(&TRALBUM_SELECTOR)
				.next()
				.context("couldn't find tralbum info on page")?
				.attr("data-tralbum")
				.context("missing data-tralbum?")?,
		)?;
		// println!("{}", tralbum);

		// let pagedata = parse_pagedata(&document)?;
		// println!("{}", pagedata);

		let item_type = ItemType::from_json(&ldjson).context("invalid item type")?;

		let album_release = if item_type == ItemType::Album {
			static PATH_TO_ALBUMRELEASE: LazyLock<JsonPath> =
				LazyLock::new(|| JsonPath::parse("$.albumRelease[0]").unwrap());
			PATH_TO_ALBUMRELEASE.query(&ldjson).exactly_one()
		} else {
			// TODO: handle track->album thing better here by checking if @id == albumRelease@id...
			// check inAlbum.albumReleaseType == "SingleRelease"
			static PATH_TO_ALBUMRELEASE: LazyLock<JsonPath> =
				LazyLock::new(|| JsonPath::parse("$.inAlbum.albumRelease[0]").unwrap());
			PATH_TO_ALBUMRELEASE.query(&ldjson).exactly_one()
		}
		.context("missing albumRelease")?;

		//println!("{album_release}");

		static PATH_TO_ITEM_ID: LazyLock<JsonPath> =
			LazyLock::new(|| JsonPath::parse("$.additionalProperty[?@.name == 'item_id'].value").unwrap());
		let item_id = PATH_TO_ITEM_ID
			.query(album_release)
			.exactly_one()
			.context("item_id missing?")?
			.as_i64()
			.unwrap();

		let free_download = if let serde_json::Value::String(s) = &tralbum["freeDownloadPage"] {
			Some(FreeDownload::Page(s.parse()?))
		} else if tralbum["current"]["require_email"].as_i64().unwrap_or(0) == 1 {
			Some(FreeDownload::RequiresEmail)
		} else {
			// not available for free download it seems...
			None
		};
		// TODO: shit looking...
		#[allow(clippy::collapsible_else_if)]
		let free_download = if item_type == ItemType::Album {
			if ldjson.get("numTracks").unwrap() == 0 {
				None
			} else {
				free_download
			}
		} else {
			if ldjson.get("inAlbum").unwrap().get("numTracks").unwrap() == 0 {
				None
			} else {
				free_download
			}
		};

		let name = album_release
			.get("name")
			.context("albumRelease missing name?")?
			.as_str()
			.unwrap()
			.to_owned();
		// let name = fucky_sanitize_basename_for_windows(&name);

		let publisher_name = ldjson
			.get("publisher")
			.context("missing publisher info?")?
			.get("name")
			.context("missing publisher name?")?
			.as_str()
			.unwrap()
			.to_owned();
		// let publisher_name = fucky_sanitize_basename_for_windows(&publisher_name);

		let artist_name = ldjson
			.get("byArtist")
			.context("missing byArtist?")?
			.get("name")
			.context("missing byArtist name?")?
			.as_str()
			.unwrap()
			.to_owned();
		// let artist_name = fucky_sanitize_basename_for_windows(&artist_name);

		// "30 Jan 2022 00:00:00 GMT"
		// https://docs.rs/jiff/latest/jiff/fmt/strtime/index.html
		let mut published = jiff::fmt::strtime::parse(
			"%d %b %Y %T",
			ldjson
				.get("datePublished")
				.context("missing datePublished?")?
				.as_str()
				.unwrap()
				.trim_end_matches(" GMT"),
		)?;
		published.set_offset(Some(jiff::tz::Offset::UTC));
		let published = published.to_timestamp()?;

		Ok(ItemInfo {
			item_type,
			item_id,
			free_download,
			name,
			publisher_name,
			artist_name,
			published,
		})
	}
}

async fn download_item(
	client: &reqwest::Client,
	item_url: &reqwest::Url,
	music_folder: &std::path::Path,
) -> anyhow::Result<()> {
	let item_html = if let Ok(path) = std::env::var("TEST_HTML") {
		tokio::fs::read_to_string(path).await?
	} else {
		client.get(item_url.clone()).send().await?.text().await?
	};
	//println!("{html}");

	let item_info = ItemInfo::parse(&item_html)?;

	// println!("{item_info:#?}");

	let publisher_folder = if ARGS.no_artist_subfolder.unwrap_or(false) {
		music_folder.to_path_buf()
	} else {
		music_folder.join(fucky_sanitize_basename_for_windows(&item_info.publisher_name))
	};
	tokio::fs::create_dir_all(&publisher_folder).await?;

	let download_path = get_file_download_path(&item_info, &publisher_folder);

	if download_path.exists() || download_path.with_extension("").exists() {
		println!("  {} already exists!", download_path.display());
		return Ok(());
	}

	if item_info.free_download.is_none() {
		println!("  NO FREE DOWNLOAD");
		println!("  NO FREE DOWNLOAD");
		println!("  NO FREE DOWNLOAD");
		println!("  NO FREE DOWNLOAD");
		return Ok(());
	}

	let download_page_url = if let Some(FreeDownload::Page(url)) = &item_info.free_download {
		url.clone()
	} else {
		const API_BASE_URL: &str = "https://www.1secmail.com/api/v1/";
		let random_email: serde_json::Value = client
			.get(format!("{API_BASE_URL}?action=genRandomMailbox&count=1"))
			.send()
			.await?
			.json()
			.await?;
		let random_email = random_email[0].as_str().unwrap();
		println!("  using email {}", random_email);

		let item_id = item_info.item_id.to_string();
		let form = std::collections::HashMap::from([
			("encoding_name", "none"),
			("item_id", &item_id),
			(
				"item_type",
				if item_info.item_type == ItemType::Album {
					"album"
				} else {
					"track"
				},
			),
			("address", random_email),
			("country", "US"),
			("postcode", "0"),
		]);

		let mut email_form_url = item_url.clone();
		email_form_url.set_path("/email_download");
		let resp: serde_json::Value = client.post(email_form_url).form(&form).send().await?.json().await?;
		if !resp["ok"].as_bool().unwrap() {
			bail!("failed to download with email...");
		}

		let mut url = None;
		for _ in 0..120 {
			tokio::time::sleep(std::time::Duration::from_secs(1)).await;
			let messages: serde_json::Value = client
				.get(format!(
					"{API_BASE_URL}?action=getMessages&login={}&domain={}",
					random_email.split("@").nth(0).unwrap(),
					random_email.split("@").nth(1).unwrap(),
				))
				.send()
				.await?
				.json()
				.await?;
			if let Some(message) = messages
				.as_array()
				.unwrap()
				.iter()
				.find(|m| m["from"].as_str().unwrap().ends_with("bandcamp.com"))
			{
				if message["from"].as_str().unwrap().ends_with("bandcamp.com") {
					let resp: serde_json::Value = client
						.get(format!(
							"{API_BASE_URL}?action=readMessage&login={}&domain={}&id={}",
							random_email.split("@").nth(0).unwrap(),
							random_email.split("@").nth(1).unwrap(),
							message["id"].as_i64().unwrap()
						))
						.send()
						.await?
						.json()
						.await?;
					let email_document = scraper::Html::parse_document(resp["htmlBody"].as_str().unwrap());
					static A_SELECTOR: LazyLock<scraper::Selector> =
						LazyLock::new(|| scraper::Selector::parse("a").unwrap());
					url = Some(reqwest::Url::parse(
						email_document.select(&A_SELECTOR).next().unwrap().attr("href").unwrap(),
					)?);
					break;
				}
			}
		}

		if url.is_none() {
			bail!("email download failed!!!!!!");
		}

		url.unwrap()
	};

	let download_page_html = client.get(download_page_url).send().await?.text().await?;
	let download_page_document = scraper::Html::parse_document(&download_page_html);
	let pagedata = parse_pagedata(&download_page_document)?;
	// println!("{}", serde_json::to_string_pretty(&pagedata)?);

	static PATH_TO_FLAC_QUERY_URL: LazyLock<JsonPath> =
		LazyLock::new(|| JsonPath::parse("$.download_items[0].downloads.flac.url").unwrap());
	let mut flac_query_url = PATH_TO_FLAC_QUERY_URL
		.query(&pagedata)
		.exactly_one()
		.context("couldn't find download url for flac?")?
		.as_str()
		.unwrap()
		.to_owned();

	println!("  waiting for download to be ready...");
	let flac_download_url = loop {
		flac_query_url = flac_query_url.replace("/download/", "/statdownload/") + "&.vrs=1";
		// println!("flac_query_url='{flac_query_url}'");
		let query_json: serde_json::Value = client
			.get(&flac_query_url)
			.header("Accept", "application/json, text/javascript")
			.send()
			.await?
			.json()
			.await?;
		// println!("\n{}\n", serde_json::to_string_pretty(&query_json).unwrap());
		if query_json["result"] == "ok" {
			break query_json["download_url"].as_str().unwrap().to_string();
		} else {
			flac_query_url = query_json["retry_url"].as_str().unwrap().to_string();
			continue;
		}
	};

	// Copied from https://gist.github.com/giuliano-macedo/4d11d6b3bb003dba3a1b53f43d81b30d
	let resp = client.get(&flac_download_url).send().await?;
	let total_size = resp.content_length().context("no content_length for download?")?;
	//
	let pb = indicatif::ProgressBar::new(total_size);
	pb.set_style(indicatif::ProgressStyle::default_bar()
		.template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")?
		.progress_chars("#>-"));
	pb.set_message(format!("Downloading {}", flac_download_url.as_str()));

	let mut downloaded = 0;
	let mut stream = resp.bytes_stream();

	let mut file = tokio::fs::File::create_new(&download_path).await?;

	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<bytes::Bytes>();
	let file_writer = tokio::spawn(async move {
		while let Some(chunk) = rx.recv().await {
			file.write_all(&chunk).await.unwrap();
		}
	});
	while let Some(item) = stream.next().await {
		let chunk = item?;
		let new = std::cmp::min(downloaded + (chunk.len() as u64), total_size);
		tx.send(chunk)?;
		downloaded = new;
		pb.set_position(new);
	}
	// pb.f(format!("Flushing file {}...", download_path.display()));
	drop(tx);
	file_writer.await?;

	pb.finish_with_message(format!("  Downloaded {}", download_path.display()));
	println!("  finished");

	Ok(())
}
