use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use regex::Regex;
use scraper::{Html, Selector};
use serde::Serialize;
use url::Url;

const DEFAULT_START_URL: &str = "https://www.safaribookings.com/tours";
const USER_AGENT: &str = "rsbin-safari-tours/0.1 (+https://github.com/zhzy0077/rsbin)";

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Crawl SafariBookings tours and filter for July migration private tours"
)]
struct Cli {
    /// First listing page to crawl.
    #[arg(long, default_value = DEFAULT_START_URL)]
    start_url: String,

    /// Maximum listing pages to crawl.
    #[arg(long, default_value_t = 5)]
    max_pages: usize,

    /// Maximum tour detail pages to inspect.
    #[arg(long, default_value_t = 50)]
    max_tours: usize,

    /// Delay between HTTP requests in milliseconds.
    #[arg(long, default_value_t = 750)]
    delay_ms: u64,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    output: OutputFormat,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Json,
    Jsonl,
}

#[derive(Debug, Serialize)]
struct TourMatch {
    title: String,
    url: String,
    price_usd: u32,
    accommodation_level: String,
    has_central_serengeti: bool,
    route_assessment: String,
    green_keywords: Vec<String>,
    score: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum TourDecision {
    Keep(AnalyzedTour),
    Drop(String),
}

#[derive(Debug, PartialEq, Eq)]
struct AnalyzedTour {
    price_usd: u32,
    accommodation_level: String,
    has_central_serengeti: bool,
    route_assessment: String,
    green_keywords: Vec<String>,
    score: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()?;

    let matches = crawl(&client, &cli).await?;
    match cli.output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&matches)?),
        OutputFormat::Jsonl => {
            for tour in matches {
                println!("{}", serde_json::to_string(&tour)?);
            }
        }
    }

    Ok(())
}

async fn crawl(client: &reqwest::Client, cli: &Cli) -> Result<Vec<TourMatch>> {
    let mut listing_queue = VecDeque::from([Url::parse(&cli.start_url)?]);
    let mut seen_listing_pages = BTreeSet::new();
    let mut seen_tours = BTreeSet::new();
    let mut matches = Vec::new();

    while let Some(listing_url) = listing_queue.pop_front() {
        if seen_listing_pages.len() >= cli.max_pages || !seen_listing_pages.insert(listing_url.clone())
        {
            continue;
        }

        let listing_html = fetch_text(client, listing_url.as_str()).await?;
        let listing_doc = Html::parse_document(&listing_html);

        for tour_url in extract_tour_links(&listing_doc, &listing_url) {
            if seen_tours.len() >= cli.max_tours {
                break;
            }
            if !seen_tours.insert(tour_url.clone()) {
                continue;
            }

            tokio::time::sleep(Duration::from_millis(cli.delay_ms)).await;
            let detail_html = fetch_text(client, tour_url.as_str()).await?;
            let detail_doc = Html::parse_document(&detail_html);
            let title = extract_title(&detail_doc).unwrap_or_else(|| tour_url.to_string());

            if let TourDecision::Keep(analyzed) = analyze_tour(&detail_html) {
                matches.push(TourMatch {
                    title,
                    url: tour_url.to_string(),
                    price_usd: analyzed.price_usd,
                    accommodation_level: analyzed.accommodation_level,
                    has_central_serengeti: analyzed.has_central_serengeti,
                    route_assessment: analyzed.route_assessment,
                    green_keywords: analyzed.green_keywords,
                    score: analyzed.score,
                });
            }
        }

        for next_url in extract_next_listing_links(&listing_doc, &listing_url) {
            if !seen_listing_pages.contains(&next_url) {
                listing_queue.push_back(next_url);
            }
        }

        tokio::time::sleep(Duration::from_millis(cli.delay_ms)).await;
    }

    matches.sort_by(|a, b| b.score.cmp(&a.score).then(a.price_usd.cmp(&b.price_usd)));
    Ok(matches)
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("request {url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {url}"))?;
    response
        .text()
        .await
        .with_context(|| format!("read response body from {url}"))
}

fn extract_tour_links(document: &Html, base_url: &Url) -> Vec<Url> {
    let link_selector = Selector::parse("a[href]").expect("valid selector");
    let mut links = BTreeSet::new();

    for link in document.select(&link_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Ok(mut url) = base_url.join(href) else {
            continue;
        };
        url.set_fragment(None);

        let path = url.path();
        if url.domain() == base_url.domain()
            && path.starts_with("/tours/")
            && path.trim_end_matches('/') != "/tours"
            && !path.contains("/operators/")
        {
            links.insert(url);
        }
    }

    links.into_iter().collect()
}

fn extract_next_listing_links(document: &Html, base_url: &Url) -> Vec<Url> {
    let link_selector = Selector::parse("a[href]").expect("valid selector");
    let mut links = BTreeSet::new();

    for link in document.select(&link_selector) {
        let text = normalized_text(link.text());
        let rel_next = link
            .value()
            .attr("rel")
            .is_some_and(|rel| rel.split_whitespace().any(|part| part.eq_ignore_ascii_case("next")));
        if !rel_next && text.to_ascii_lowercase() != "next" && text != "›" && text != ">" {
            continue;
        }

        if let Some(href) = link.value().attr("href") {
            if let Ok(mut url) = base_url.join(href) {
                url.set_fragment(None);
                if url.domain() == base_url.domain() && url.path().starts_with("/tours") {
                    links.insert(url);
                }
            }
        }
    }

    links.into_iter().collect()
}

fn extract_title(document: &Html) -> Option<String> {
    for selector in ["h1", "title"] {
        let selector = Selector::parse(selector).expect("valid selector");
        if let Some(element) = document.select(&selector).next() {
            let title = normalized_text(element.text());
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    None
}

fn analyze_tour(html: &str) -> TourDecision {
    let document = Html::parse_document(html);
    let page_text = normalized_text(document.root_element().text());
    let lower = page_text.to_ascii_lowercase();

    let Some(price_usd) = extract_price_usd(&page_text) else {
        return TourDecision::Drop("missing USD price".to_string());
    };
    if price_usd > 3000 {
        return TourDecision::Drop(format!("price {price_usd} exceeds 3000 USD"));
    }

    if !contains_phrase(&lower, "private tour") {
        return TourDecision::Drop("not a private tour".to_string());
    }
    if contains_phrase(&lower, "shared tour") {
        return TourDecision::Drop("contains shared tour".to_string());
    }

    let Some(accommodation_level) = extract_accommodation_level(&lower) else {
        return TourDecision::Drop("missing mid-range/luxury accommodation".to_string());
    };
    if contains_phrase(&lower, "budget") || camping_only(&lower) {
        return TourDecision::Drop("contains budget or camping-only accommodation".to_string());
    }

    if !contains_phrase(&lower, "northern serengeti np") {
        return TourDecision::Drop("route misses Northern Serengeti NP".to_string());
    }
    let has_central_serengeti = contains_phrase(&lower, "central serengeti np");
    let route_assessment = if has_central_serengeti {
        "Central + Northern Serengeti route".to_string()
    } else {
        "Northern-only route: 魔鬼车程 risk".to_string()
    };

    if northern_day_has_red_lodging(&page_text) {
        return TourDecision::Drop(
            "Northern Serengeti day uses Ikoma/Grumeti/Western lodging".to_string(),
        );
    }

    if contains_excluded_shared_activity_terms(&lower) {
        return TourDecision::Drop("private tour has shared activity terms".to_string());
    }
    if !airport_transfer_included(&lower) {
        return TourDecision::Drop("airport transfer is not explicitly included".to_string());
    }

    let green_keywords = extract_green_keywords(&lower);
    let mut score = 100;
    if has_central_serengeti {
        score += 30;
    }
    score += (green_keywords.len() as u32) * 20;
    score += (3000_u32.saturating_sub(price_usd)) / 100;

    TourDecision::Keep(AnalyzedTour {
        price_usd,
        accommodation_level,
        has_central_serengeti,
        route_assessment,
        green_keywords,
        score,
    })
}

fn extract_price_usd(text: &str) -> Option<u32> {
    let price_regex = Regex::new(r"(?i)(?:US\$|USD|\$)\s*([0-9][0-9,]*)").expect("valid regex");
    price_regex.captures_iter(text).find_map(|capture| {
        capture
            .get(1)
            .and_then(|m| m.as_str().replace(',', "").parse::<u32>().ok())
    })
}

fn extract_accommodation_level(lower_text: &str) -> Option<String> {
    let mid_range = contains_phrase(lower_text, "mid-range");
    let luxury = contains_phrase(lower_text, "luxury");

    match (mid_range, luxury) {
        (true, true) => Some("Mid-range/Luxury".to_string()),
        (true, false) => Some("Mid-range".to_string()),
        (false, true) => Some("Luxury".to_string()),
        (false, false) => None,
    }
}

fn camping_only(lower_text: &str) -> bool {
    contains_phrase(lower_text, "camping")
        && !contains_phrase(lower_text, "mid-range")
        && !contains_phrase(lower_text, "luxury")
}

fn northern_day_has_red_lodging(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let day_regex = Regex::new(r"(?i)(?:^|\b)(day\s+\d+[^\n]*)").expect("valid regex");
    let starts: Vec<_> = day_regex.find_iter(text).map(|m| m.start()).collect();

    if starts.is_empty() {
        return contains_phrase(&lower, "northern serengeti np")
            && red_lodging_keyword_found(&lower);
    }

    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        let day = text[*start..end].to_ascii_lowercase();
        if contains_phrase(&day, "northern serengeti np") && red_lodging_keyword_found(&day) {
            return true;
        }
    }

    false
}

fn red_lodging_keyword_found(lower_text: &str) -> bool {
    ["ikoma", "grumeti", "western"]
        .iter()
        .any(|keyword| contains_phrase(lower_text, keyword))
}

fn extract_green_keywords(lower_text: &str) -> Vec<String> {
    ["kogatende", "mara river", "mobile camp", "migration camp"]
        .iter()
        .filter(|keyword| contains_phrase(lower_text, keyword))
        .map(|keyword| (*keyword).to_string())
        .collect()
}

fn contains_excluded_shared_activity_terms(lower_text: &str) -> bool {
    contains_phrase(lower_text, "shared with others")
        || (contains_phrase(lower_text, "wildlife viewing activities are run by the lodges")
            && contains_phrase(lower_text, "shared"))
}

fn airport_transfer_included(lower_text: &str) -> bool {
    contains_phrase(lower_text, "transfer from and back to the airport is included")
}

fn contains_phrase(lower_text: &str, phrase: &str) -> bool {
    lower_text.contains(&phrase.to_ascii_lowercase())
}

fn normalized_text<'a>(text: impl Iterator<Item = &'a str>) -> String {
    text.collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_html(extra: &str) -> String {
        format!(
            r#"
            <html><head><title>7-Day Great Migration Safari</title></head><body>
              <h1>7-Day Great Migration Safari</h1>
              <div>US$2,950</div>
              <div>Private tour</div>
              <div>Luxury accommodation</div>
              <section><h2>You Visit</h2>
                <p>Central Serengeti NP, Northern Serengeti NP</p>
              </section>
              <section><h2>Day 3: Northern Serengeti NP</h2>
                <p>Accommodation: Kogatende Mobile Camp near Mara River</p>
              </section>
              <section><h2>Tour Features</h2>
                <p>Transfer from and back to the airport is included.</p>
              </section>
              {extra}
            </body></html>
            "#
        )
    }

    #[test]
    fn keeps_private_migration_tour_with_green_camp() {
        let decision = analyze_tour(&base_html(""));
        let TourDecision::Keep(tour) = decision else {
            panic!("expected keep");
        };

        assert_eq!(tour.price_usd, 2950);
        assert_eq!(tour.accommodation_level, "Luxury");
        assert!(tour.has_central_serengeti);
        assert_eq!(
            tour.green_keywords,
            vec!["kogatende", "mara river", "mobile camp"]
        );
    }

    #[test]
    fn drops_shared_tours_even_when_private_text_exists() {
        let decision = analyze_tour(&base_html("<p>Shared tour option available</p>"));
        assert!(matches!(decision, TourDecision::Drop(reason) if reason == "contains shared tour"));
    }

    #[test]
    fn drops_red_lodging_on_northern_serengeti_day() {
        let html = base_html(
            r#"<section><h2>Day 4: Northern Serengeti NP</h2>
               <p>Accommodation: Ikoma tented camp in the Western corridor</p></section>"#,
        );
        let decision = analyze_tour(&html);
        assert!(matches!(decision, TourDecision::Drop(reason) if reason.contains("Ikoma")));
    }

    #[test]
    fn drops_missing_airport_transfer() {
        let html = base_html("").replace(
            "Transfer from and back to the airport is included.",
            "Airport transfers can be arranged at extra cost.",
        );
        let decision = analyze_tour(&html);
        assert!(matches!(decision, TourDecision::Drop(reason) if reason == "airport transfer is not explicitly included"));
    }

    #[test]
    fn flags_northern_only_route_as_drive_risk() {
        let html = base_html("").replace("Central Serengeti NP, ", "");
        let decision = analyze_tour(&html);
        let TourDecision::Keep(tour) = decision else {
            panic!("expected keep");
        };

        assert!(!tour.has_central_serengeti);
        assert!(tour.route_assessment.contains("魔鬼车程"));
    }

    #[test]
    fn extracts_tour_links_from_listing() {
        let base_url = Url::parse("https://www.safaribookings.com/tours").unwrap();
        let document = Html::parse_document(
            r#"<a href="/tours/t123/example">Tour</a><a href="/tours">Index</a>"#,
        );

        let links = extract_tour_links(&document, &base_url);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].as_str(), "https://www.safaribookings.com/tours/t123/example");
    }
}
