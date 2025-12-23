// SPDX-License-Identifier: MIT
// Copyright 2024-2025 rtldg <rtldg@protonmail.com>

use std::{
	collections::HashSet,
	path::{Path, PathBuf},
};

pub async fn lets_fucking_go(folder: &str, url: &str) -> anyhow::Result<()> {
	let folder: PathBuf = folder.parse()?;
	let url = reqwest::Url::parse(url)?;

	let mut items = HashSet::new();
	let mut artists = HashSet::new();
	artists.insert(url);

	let chrome_user_agent = {
		format!(
			"{}/{} ({})",
			env!("CARGO_PKG_NAME"),
			env!("CARGO_PKG_VERSION"),
			env!("CARGO_PKG_REPOSITORY")
		)
	};

	let client = reqwest::ClientBuilder::new()
		.user_agent(chrome_user_agent)
		.build()
		.unwrap();

	for artist in &artists {
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		println!("Checking for items from {}", artist.as_str());

		items.extend(crate::get_items_from_artist(&client, artist).await?.into_iter());
	}

	for item in &items {
		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
		println!("Checking {}", item.as_str());
		maybe_move_item(&client, item, &folder).await?;
	}

	Ok(())
}

async fn maybe_move_item(client: &reqwest::Client, item_url: &reqwest::Url, folder: &Path) -> anyhow::Result<()> {
	let item_html = if let Ok(path) = std::env::var("TEST_HTML") {
		tokio::fs::read_to_string(path).await?
	} else {
		client.get(item_url.clone()).send().await?.text().await?
	};

	let item_info = crate::ItemInfo::parse(&item_html)?;

	let undated_path = folder.join(format!("{} - {}", item_info.artist_name, item_info.name));

	if undated_path.exists() {
		let dated_path = format!(
			"{} - {} - {}",
			item_info.published.strftime("%Y-%m-%d"),
			item_info.artist_name,
			item_info.name
		);
		let to = folder.join(dated_path);

		if to.exists() {
			println!("wtf {}", to.display());
		} else {
			println!("  moving\n  {}\n  to\n  {}", undated_path.display(), to.display());
			tokio::fs::rename(undated_path, to).await?;
		}
	}

	Ok(())
}
