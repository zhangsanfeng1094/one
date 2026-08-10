//! edit-gauntlet: hard multi-trap edit harness.
//! Prefer surgical `edit` calls. Do not rewrite whole files.

pub mod twins;
pub mod crowded;
pub mod whitespace;
pub mod crlf_mod;
pub mod rename_me;
pub mod unicode;
pub mod decoy;
pub mod near_twins;
pub mod bait;
pub mod sandwich;
pub mod expr;
pub mod shadow;

pub mod deep {
    pub mod nested;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twins_alpha_multiplies() {
        assert_eq!(twins::alpha_compute(6, 7), 42);
    }

    #[test]
    fn twins_beta_subtracts() {
        assert_eq!(twins::beta_compute(10, 3), 7);
    }

    #[test]
    fn twins_decoys_still_sum() {
        assert_eq!(twins::gamma_sum(2, 3), 5);
        assert_eq!(twins::delta_sum(4, 5), 9);
    }

    #[test]
    fn crowded_only_site_9_is_triple_step() {
        // 8× +1, then *3, then 6× +1 → 0: 8 → 24 → 30
        assert_eq!(crowded::pipeline(0), 30);
    }

    #[test]
    fn whitespace_scale_pair() {
        assert_eq!(whitespace::scale_pair(5, 1), 11); // 5*2+1
    }

    #[test]
    fn crlf_triple() {
        assert_eq!(crlf_mod::triple(4), 12);
    }

    #[test]
    fn rename_struct_and_label() {
        let w = rename_me::CanonicalWidget::new(3);
        assert_eq!(w.label(), "CanonicalWidget#3");
        let (a, b) = rename_me::make_pair();
        assert_eq!(a.id, 1);
        assert_eq!(b.bump().id, 3);
        assert!(rename_me::describe(&rename_me::CanonicalWidget::new(9)).contains("CanonicalWidget"));
    }

    #[test]
    fn unicode_motto_and_score() {
        assert_eq!(unicode::motto(), "\u{201c}gauntlet\u{201d}");
        assert_eq!(unicode::score_line(3), "val=30");
    }

    #[test]
    fn nested_bonus_multiplies() {
        assert_eq!(deep::nested::apply_bonus(4, 5), 20);
        assert_eq!(deep::nested::apply_noise(4, 5), 9);
        assert_eq!(deep::nested::apply_padding(1, 2), 3);
    }

    #[test]
    fn decoys_untouched() {
        assert_eq!(decoy::decoy_0(10), 11);
        assert_eq!(decoy::decoy_39(0), 1);
    }

    // --- new easy-to-botch edit traps ---

    #[test]
    fn near_twins_only_primary_mid_multiplies() {
        // primary(5): stage=6, mid=12, out=15
        assert_eq!(near_twins::primary_pipeline(5), 15);
        // secondary / tertiary keep mid as +2 → stage=6, mid=8, out=11
        assert_eq!(near_twins::secondary_pipeline(5), 11);
        assert_eq!(near_twins::tertiary_pipeline(5), 11);
    }

    #[test]
    fn bait_combine_multiplies_but_docs_untouched() {
        assert_eq!(bait::combine(6, 7), 42);
        assert_eq!(bait::example_sum(), 3);
        assert_eq!(bait::help_text(), "formula: left + right (docs only)");
        assert!(bait::debug_label(1, 2).starts_with("left + right"));
        let src = include_str!("bait.rs");
        assert!(
            src.contains("DO_NOT_EDIT_COMMENT: result = left + right"),
            "comment bait must remain"
        );
        assert!(src.contains("BAIT_CODE_LINE"));
    }

    #[test]
    fn sandwich_only_mid_is_times_five() {
        // layers(2): 3 + 10 + 3 = 16
        assert_eq!(sandwich::layers(2), 16);
        assert_eq!(sandwich::edge_pad(4), 10); // 5+5
        let src = include_str!("sandwich.rs");
        assert!(src.contains("SANDWICH_MID"));
        // top/bot must still use + 1
        assert!(src.contains("let top = n + 1;"));
        assert!(src.contains("let bot = n + 1;"));
    }

    #[test]
    fn expr_mixed_and_chain() {
        // (4*5)-3 = 17
        assert_eq!(expr::mixed(4, 5, 3), 17);
        // decoy stays pure sum
        assert_eq!(expr::mixed_decoy(4, 5, 3), 12);
        // n=2 → x=12, y=120, z=130
        assert_eq!(expr::scale_chain(2), 130);
        let src = include_str!("expr.rs");
        assert!(src.contains("EXPR_MIXED_TARGET"));
        assert!(src.contains("EXPR_MIXED_DECOY"));
        assert!(src.contains("EXPR_CHAIN_MID"));
    }

    #[test]
    fn shadow_inner_only() {
        // outer n+1, inner n*3 → (n+1)+(n*3) ; n=4 → 5+12=17
        assert_eq!(shadow::outer_adjust(4), 17);
        assert_eq!(shadow::outer_only(4), 5);
        let src = include_str!("shadow.rs");
        assert!(src.contains("OUTER_KEEP"));
        assert!(src.contains("INNER_FIX"));
        // outer value line must remain `n + 1`
        assert!(src.contains("let value = n + 1; // OUTER_KEEP"));
    }

    #[test]
    fn fingerprints_preserved() {
        let twins = include_str!("twins.rs");
        assert!(twins.contains("ONLY ALPHA"));
        assert!(twins.contains("ONLY BETA"));
        let crowded = include_str!("crowded.rs");
        assert!(crowded.contains("TARGET_SITE_9"));
        let crlf = include_str!("crlf_mod.rs");
        assert!(crlf.contains('\r'), "crlf_mod.rs must remain CRLF (contains CR)");
        let near = include_str!("near_twins.rs");
        assert!(near.contains("NEAR_TWIN_PRIMARY_MID"));
        assert!(near.contains("NEAR_TWIN_SECONDARY_MID"));
        assert!(near.contains("NEAR_TWIN_TERTIARY_MID"));
    }
}
