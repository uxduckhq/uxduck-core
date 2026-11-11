//! **MAVInspect** is a library for parsing MAVLink
//! [message definitions](https://github.com/mavlink/mavlink/tree/master/message_definitions/v1.0).
//!
//! <span style="font-size:24px">[🇺🇦](https://mavka.gitlab.io/home/a_note_on_the_war_in_ukraine/)</span>
//! [![`repository`](https://img.shields.io/gitlab/pipeline-status/mavka/libs/mavinspect.svg?branch=main&label=repository)](https://gitlab.com/mavka/libs/mavinspect)
//! [![`crates.io`](https://img.shields.io/crates/v/mavinspect.svg)](https://crates.io/crates/mavinspect)
//! [![`docs.rs`](https://img.shields.io/docsrs/mavinspect.svg?label=docs.rs)](https://docs.rs/mavinspect/latest/mavinspect/)
//! [![`issues`](https://img.shields.io/gitlab/issues/open/mavka/libs/mavinspect.svg)](https://gitlab.com/mavka/libs/mavinspect/-/issues/)
//!
//! # Usage
//!
//! Discover and parse MAVLink XML using [`Inspector`]:
//!
//! ```rust
//! use mavinspect::parser::Inspector;
//!
//! // Instantiate inspector and load XML definitions from several locations
//! let inspector = Inspector::builder()
//!     .set_sources(&[
//!         "./message_definitions/standard",
//!         "./message_definitions/extra",
//!     ])
//!     .set_include(&["crazy_flight"])
//!     .build()
//!     .unwrap();
//!   
//! // Parse all XML definitions
//! let protocol = inspector.parse().unwrap();
//!   
//! // Get `CrazyFlight` custom dialect
//! let crazy_flight = protocol.get_dialect_by_canonical_name("crazy_flight").unwrap();
//!
//! // Get `CRAZYFLIGHT_OUTCRY` message
//! let outcry_message = crazy_flight.get_message_by_name("CRAZYFLIGHT_OUTCRY").unwrap();
//! println!("\n`CRAZYFLIGHT_OUTCRY` message: {:#?}", outcry_message);
//! ```
//!
//! Here we obtain an instance of [`Protocol`](protocol::Protocol) that contains a collection of
//! dialects. Each [`Dialect`](protocol::Dialect) contains MAVLink [`enums`](protocol::Dialect::enums) and
//! [`messages`](protocol::Dialect::messages) as well as additional information (like version or dialect dependencies).
//!
//! # Dialects Naming
//!
//! To avoid collision between similar dialect names like `SomeDialect` and `some_dialect` in libraries which use
//! MAVInspect for code generation, we match dialect names up to `lower_snake_case`. This means that `SomeDialect`,
//! `SOME_DIALECT`, and `some_dialect` will be considered the same, while `somedialect` will be considered as a
//! different dialect. For example:
//!
//! ```rust
//! # use mavinspect::parser::Inspector;
//! let inspector_builder = Inspector::builder()
//!     // this is the original dialect name as it appears in the XML file name
//!     .set_include(&["SomeDialect"])
//!     // and this is a canonical name
//!     .set_include(&["some_dialect"]);
//! // both rules will lead to the same result since names will be converted to `some_dialect` upon matching
//! ```
//!
//! See details in [`Dialect::canonical_name`](protocol::Dialect::canonical_name).
//!
//! # Filtering
//!
//! The library provides two approaches to filter MAVLink definitions once they are loaded: the
//! first is filtering entities within a dialect, the second is filtering out dialects itself.
//!
//! ## Filter Entities
//!
//! It is possible to filter [`Protocol`](protocol::Protocol) and a specific [`Dialect`](protocol::Dialect) by messages,
//! enums, commands, and MAVLink [microservices](https://mavlink.io/en/services/). For example:
//!
//! ```rust
//! # use mavinspect::parser::Inspector;
//! # let inspector = Inspector::builder()
//! #     .set_sources(&["./message_definitions/standard"])
//! #     .set_include(&["common"])
//! #     .build().unwrap();
//! # let protocol = inspector.parse().unwrap();
//! use mavinspect::protocol::{Filter, Microservices};
//!
//! /* obtain protocol with `common` dialect */
//!
//! // Filter
//! let filtered_protocol = protocol.filtered(
//!     &Filter::by_microservices(Microservices::HEARTBEAT | Microservices::MISSION)
//!     .with_messages(&["FILE_TRANSFER_PROTOCOL"])
//!     .with_enums(&["GIMBAL_DEVICE_CAP_FLAGS"])
//!     .with_commands(&["MAV_CMD_SET_*"])
//! );
//! ```
//!
//! Check [`Filter`](protocol::Filter) for details.
//!
//! ## Filter Dialects
//!
//! To filter dialects once they are loaded, use
//! [`Protocol::with_dialects_included`](protocol::Protocol::with_dialects_included). This allows to
//! filter out dialects based on their names, tags, and dependencies.
//!
//! # Metadata & Tags
//!
//! It is possible to add metadata to dialects by placing a `.dialects-metadata.yml` file
//! into the directory with message definitions. For example:
//!
//! ```yaml
//! # Tags that will be added to all dialects
//! tags:
//!   - extra
//!   - test
//! dialects:
//!   MyDialect:
//!     # Tags that will be added only to `MyDialect` dialect
//!     tags:
//!       - foo
//! ```
//!
//! Tags may be used by downstream crates to distinguish dialects of a different kind. For example,
//! [`mavlink-dialects`](https://crates.io/crates/mavlink-dialects) uses tags to filter user-defined
//! dialects.
//!
//! Check [`MavInspectMetadata`](parser::MavInspectMetadata) for details.
//!
//! # Protocol Specification API
//!
//! MAVLink entities are defined in the [`protocol`] module. The root of the specification is
//! [`Protocol`](protocol::Protocol) struct that contains a collection of [`Dialect`](protocol::Dialect). The latter
//! contains dialect metadata, [`messages`](protocol::Dialect::messages) and [`enums`](protocol::Dialect::enums).
//!
//! Most of the entities with complex validation rules and lots of parameters are using
//! [builder](https://rust-unofficial.github.io/patterns/patterns/creational/builder.html) instead of direct
//! instantiation (no constructors). We assume that library clients prefer to use XML definitions as a ground truth for
//! their dialects and are not very interested in manual creation of Rust entities. However, this possibility remains
//! open yet not encouraged.
//!
//! All entities optionally support [Serde](https://serde.rs) and are thread-safe.
//!
//! # Errors
//!
//! All errors are variants (or sub-variants) of [`Error`](errors::Error). Fallible functions and methods return
//! [`Result`] type.
//!
//! # Feature Flags
//!
#![doc = document_features::document_features!()]
//
#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![doc(
    html_logo_url = "https://gitlab.com/mavka/libs/mavinspect/-/raw/main/avatar.png?ref_type=heads",
    html_favicon_url = "https://gitlab.com/mavka/libs/mavinspect/-/raw/main/avatar.png?ref_type=heads"
)]

pub mod parser;
#[doc(inline)]
pub use parser::Inspector;

pub mod errors;
pub mod protocol;
pub mod utils;
