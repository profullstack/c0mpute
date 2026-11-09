//! Podcasts on c0mpute (DIP-0022), built on [`crate::hosting`]:
//!
//! * [`show`] — the `podcasting` plugin: a show is a hosting site holding an
//!   RSS 2.0 + iTunes + Podcasting 2.0 `feed.xml`, an `index.html` episode
//!   page and the audio. Three self-healing copies, served by every gateway
//!   at `/s/<site_id>/feed.xml` and on DNS-verified custom domains.
//! * [`listen`] — the `podcasts` plugin: subscribe to c0mpute shows or any
//!   RSS feed, list, download and play episodes.
//! * [`rss`] — the feed reader both sides use; [`dates`] — RFC 2822 dates.

pub mod dates;
pub mod listen;
pub mod rss;
pub mod show;
