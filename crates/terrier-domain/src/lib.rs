//! terrier domain: pure types and logic shared by server, client and UI.

pub mod listing;
pub mod llm;
pub mod normalize;
pub mod search;
pub mod status;

pub use listing::{
    ExtractedAttrs, Flag, Listing, ListingDetail, ListingImage, ListingStatus, ListingWithHistory,
    Moderation, PricePoint, PropertyType, RawListing, Seller, SellerKind,
};
pub use llm::{LlmPrompts, LlmSettings, LlmSettingsUpdate};
pub use search::{Search, SearchRequest, search_matches};
pub use status::{CommuneStat, HealthResponse, LlmStatus, SourceStatus, StatusResponse, TickStats};
