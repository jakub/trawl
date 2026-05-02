use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

macro_rules! snake_case_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $Name:ident {
            $($Variant:ident => $str:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        $vis enum $Name {
            $($Variant),+
        }

        impl fmt::Display for $Name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let s = match self {
                    $(Self::$Variant => $str),+
                };
                f.write_str(s)
            }
        }

        impl FromStr for $Name {
            type Err = ParseEnumError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($str => Ok(Self::$Variant),)+
                    _ => Err(ParseEnumError {
                        type_name: stringify!($Name),
                        value: s.to_owned(),
                    }),
                }
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseEnumError {
    pub type_name: &'static str,
    pub value: String,
}

impl fmt::Display for ParseEnumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} value: {:?}", self.type_name, self.value)
    }
}

impl std::error::Error for ParseEnumError {}

snake_case_enum! {
    pub enum Polarity {
        Affirmed => "affirmed",
        Denied => "denied",
        Corrected => "corrected",
        Retracted => "retracted",
        Superseded => "superseded",
        Unknown => "unknown",
    }
}

snake_case_enum! {
    pub enum Modality {
        Observed => "observed",
        Assessed => "assessed",
        Claimed => "claimed",
        Rumored => "rumored",
        Predicted => "predicted",
        Quoted => "quoted",
        Inferred => "inferred",
    }
}

snake_case_enum! {
    pub enum StoryState {
        Emerging => "emerging",
        Active => "active",
        Monitoring => "monitoring",
        Closed => "closed",
        Superseded => "superseded",
        Debunked => "debunked",
    }
}

snake_case_enum! {
    pub enum StoryClass {
        Vulnerability => "vulnerability",
        Campaign => "campaign",
        ActorProfile => "actor_profile",
        MalwareProfile => "malware_profile",
        Incident => "incident",
        Policy => "policy",
        General => "general",
    }
}

snake_case_enum! {
    pub enum ClaimRelation {
        Supports => "supports",
        Contradicts => "contradicts",
        Supersedes => "supersedes",
        Refines => "refines",
        Duplicates => "duplicates",
    }
}

snake_case_enum! {
    pub enum StoryClaimRelationship {
        Evidence => "evidence",
        Duplicate => "duplicate",
        Evolution => "evolution",
        Related => "related",
        Background => "background",
        NewStory => "new_story",
        Correction => "correction",
        Contradiction => "contradiction",
        Supersession => "supersession",
    }
}

snake_case_enum! {
    pub enum StoryRelation {
        Duplicate => "duplicate",
        Evolution => "evolution",
        Related => "related",
        Background => "background",
        ParentChild => "parent_child",
        Supersession => "supersession",
    }
}

snake_case_enum! {
    pub enum SourceState {
        Active => "active",
        Paused => "paused",
        Disabled => "disabled",
        Archived => "archived",
    }
}

snake_case_enum! {
    pub enum SourceClass {
        VendorAdvisory => "vendor_advisory",
        News => "news",
        Government => "government",
        LeakSite => "leak_site",
        SecurityVendor => "security_vendor",
        Researcher => "researcher",
        Aggregator => "aggregator",
        SocialMedia => "social_media",
    }
}

snake_case_enum! {
    pub enum FeedKind {
        Rss => "rss",
        Atom => "atom",
        JsonFeed => "json_feed",
        Syndication => "syndication",
    }
}

snake_case_enum! {
    pub enum QueueItemType {
        CandidateMerge => "candidate_merge",
        MaterialDelta => "material_delta",
        Contradiction => "contradiction",
        ManualReview => "manual_review",
    }
}

snake_case_enum! {
    pub enum QueueItemStatus {
        Pending => "pending",
        Assigned => "assigned",
        Resolved => "resolved",
        Expired => "expired",
    }
}

snake_case_enum! {
    pub enum AnalystAction {
        Approve => "approve",
        Reject => "reject",
        Attach => "attach",
    }
}

snake_case_enum! {
    pub enum TimelineOrigin {
        Automated => "automated",
        Analyst => "analyst",
        Backfill => "backfill",
    }
}

snake_case_enum! {
    pub enum AliasKind {
        Exact => "exact",
        Abbreviation => "abbreviation",
        Translation => "translation",
        FormerName => "former_name",
        Typo => "typo",
        StixId => "stix_id",
        MispUuid => "misp_uuid",
    }
}

snake_case_enum! {
    pub enum EntityType {
        ThreatActor => "threat_actor",
        Malware => "malware",
        Product => "product",
        Organisation => "organisation",
        Person => "person",
        Technique => "technique",
        Sector => "sector",
        Country => "country",
        Tool => "tool",
        Vulnerability => "vulnerability",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! roundtrip_test {
        ($name:ident, $Type:ty, [$($variant:expr),+ $(,)?]) => {
            #[test]
            fn $name() {
                for variant in [$($variant),+] {
                    let s = variant.to_string();
                    let parsed: $Type = s.parse().unwrap();
                    assert_eq!(parsed, variant);

                    let json = serde_json::to_string(&variant).unwrap();
                    let from_json: $Type = serde_json::from_str(&json).unwrap();
                    assert_eq!(from_json, variant);
                }
            }
        };
    }

    roundtrip_test!(
        polarity_roundtrip,
        Polarity,
        [
            Polarity::Affirmed,
            Polarity::Denied,
            Polarity::Corrected,
            Polarity::Retracted,
            Polarity::Superseded,
            Polarity::Unknown,
        ]
    );

    roundtrip_test!(
        modality_roundtrip,
        Modality,
        [
            Modality::Observed,
            Modality::Assessed,
            Modality::Claimed,
            Modality::Rumored,
            Modality::Predicted,
            Modality::Quoted,
            Modality::Inferred,
        ]
    );

    roundtrip_test!(
        story_state_roundtrip,
        StoryState,
        [
            StoryState::Emerging,
            StoryState::Active,
            StoryState::Monitoring,
            StoryState::Closed,
            StoryState::Superseded,
            StoryState::Debunked,
        ]
    );

    roundtrip_test!(
        story_class_roundtrip,
        StoryClass,
        [
            StoryClass::Vulnerability,
            StoryClass::Campaign,
            StoryClass::ActorProfile,
            StoryClass::MalwareProfile,
            StoryClass::Incident,
            StoryClass::Policy,
            StoryClass::General,
        ]
    );

    roundtrip_test!(
        claim_relation_roundtrip,
        ClaimRelation,
        [
            ClaimRelation::Supports,
            ClaimRelation::Contradicts,
            ClaimRelation::Supersedes,
            ClaimRelation::Refines,
            ClaimRelation::Duplicates,
        ]
    );

    roundtrip_test!(
        story_claim_relationship_roundtrip,
        StoryClaimRelationship,
        [
            StoryClaimRelationship::Evidence,
            StoryClaimRelationship::Duplicate,
            StoryClaimRelationship::Evolution,
            StoryClaimRelationship::Related,
            StoryClaimRelationship::Background,
            StoryClaimRelationship::NewStory,
            StoryClaimRelationship::Correction,
            StoryClaimRelationship::Contradiction,
            StoryClaimRelationship::Supersession,
        ]
    );

    roundtrip_test!(
        story_relation_roundtrip,
        StoryRelation,
        [
            StoryRelation::Duplicate,
            StoryRelation::Evolution,
            StoryRelation::Related,
            StoryRelation::Background,
            StoryRelation::ParentChild,
            StoryRelation::Supersession,
        ]
    );

    roundtrip_test!(
        source_state_roundtrip,
        SourceState,
        [
            SourceState::Active,
            SourceState::Paused,
            SourceState::Disabled,
            SourceState::Archived,
        ]
    );

    roundtrip_test!(
        source_class_roundtrip,
        SourceClass,
        [
            SourceClass::VendorAdvisory,
            SourceClass::News,
            SourceClass::Government,
            SourceClass::LeakSite,
            SourceClass::SecurityVendor,
            SourceClass::Researcher,
            SourceClass::Aggregator,
            SourceClass::SocialMedia,
        ]
    );

    roundtrip_test!(
        feed_kind_roundtrip,
        FeedKind,
        [
            FeedKind::Rss,
            FeedKind::Atom,
            FeedKind::JsonFeed,
            FeedKind::Syndication,
        ]
    );

    roundtrip_test!(
        queue_item_type_roundtrip,
        QueueItemType,
        [
            QueueItemType::CandidateMerge,
            QueueItemType::MaterialDelta,
            QueueItemType::Contradiction,
            QueueItemType::ManualReview,
        ]
    );

    roundtrip_test!(
        queue_item_status_roundtrip,
        QueueItemStatus,
        [
            QueueItemStatus::Pending,
            QueueItemStatus::Assigned,
            QueueItemStatus::Resolved,
            QueueItemStatus::Expired,
        ]
    );

    roundtrip_test!(
        analyst_action_roundtrip,
        AnalystAction,
        [
            AnalystAction::Approve,
            AnalystAction::Reject,
            AnalystAction::Attach,
        ]
    );

    roundtrip_test!(
        timeline_origin_roundtrip,
        TimelineOrigin,
        [
            TimelineOrigin::Automated,
            TimelineOrigin::Analyst,
            TimelineOrigin::Backfill,
        ]
    );

    roundtrip_test!(
        alias_kind_roundtrip,
        AliasKind,
        [
            AliasKind::Exact,
            AliasKind::Abbreviation,
            AliasKind::Translation,
            AliasKind::FormerName,
            AliasKind::Typo,
            AliasKind::StixId,
            AliasKind::MispUuid,
        ]
    );

    roundtrip_test!(
        entity_type_roundtrip,
        EntityType,
        [
            EntityType::ThreatActor,
            EntityType::Malware,
            EntityType::Product,
            EntityType::Organisation,
            EntityType::Person,
            EntityType::Technique,
            EntityType::Sector,
            EntityType::Country,
            EntityType::Tool,
            EntityType::Vulnerability,
        ]
    );

    #[test]
    fn invalid_value_produces_error() {
        let err = "bogus".parse::<Polarity>().unwrap_err();
        assert_eq!(err.type_name, "Polarity");
        assert_eq!(err.value, "bogus");
        assert!(err.to_string().contains("bogus"));
    }
}
