#!/usr/bin/env bash
# Run only the explicitly named GTK/VTE regressions. Each test gets a fresh
# process, display, and session bus; sweeping `--ignored` would also execute
# manual diagnostics and micro-benchmarks that are not CI gates.

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

# A developer's font scale, density, history, or theme must not change the
# geometry asserted by these regressions. Give the suite an empty, private XDG
# tree just as CI gets from a fresh account; Cargo/Nix keep their own homes.
test_xdg_root="$(mktemp -d)"
cleanup() {
    rm -rf -- "${test_xdg_root}"
}
trap cleanup EXIT
mkdir -p \
    "${test_xdg_root}/config" \
    "${test_xdg_root}/data" \
    "${test_xdg_root}/state" \
    "${test_xdg_root}/cache" \
    "${test_xdg_root}/runtime"
chmod 700 "${test_xdg_root}/runtime"
export XDG_CONFIG_HOME="${test_xdg_root}/config"
export XDG_DATA_HOME="${test_xdg_root}/data"
export XDG_STATE_HOME="${test_xdg_root}/state"
export XDG_CACHE_HOME="${test_xdg_root}/cache"
export XDG_RUNTIME_DIR="${test_xdg_root}/runtime"

for command in dbus-run-session xvfb-run; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        printf 'Error: %s is required for display-backed tests.\n' "${command}" >&2
        exit 1
    fi
done

# Nix's dbus-daemon keeps its session policy in the store instead of `/etc`.
# Pass the policy beside the selected daemon when available; distro installs
# continue to use their ordinary `/etc/dbus-1/session.conf` fallback.
dbus_run_args=()
dbus_daemon="$(command -v dbus-daemon)"
dbus_prefix="${dbus_daemon%/bin/dbus-daemon}"
dbus_store_config="${dbus_prefix}/share/dbus-1/session.conf"
if [[ -r "${dbus_store_config}" ]]; then
    dbus_run_args+=("--config-file=${dbus_store_config}")
elif [[ ! -r /etc/dbus-1/session.conf ]]; then
    printf 'Error: no D-Bus session configuration is readable.\n' >&2
    exit 1
fi

tests=(
    ui::dialogs::tests::cross_block_search_compact_rows_are_inside_horizontal_scroller
    ui::dialogs::tests::cross_block_search_close_borrows_the_gtk_dialog_without_releasing_its_slot
    ui::panes::tests::equalize_without_allocation_leaves_the_tree_untouched
    block_view::blocks::tests::diag_short_ls_block_geometry
    block_view::blocks::tests::block_density_switches_on_widgets_that_already_exist
    block_view::find::tests::a_card_vte_must_drop_its_previous_anchor_before_a_fresh_query
    block_view::find::tests::unified_vte_fresh_query_reaches_scrollback_before_a_prior_match
    block_view::find::tests::unified_bounded_and_native_fallback_prefer_visible_match_with_huge_old_scrollback
    block_view::find::tests::unified_complete_windows_step_visible_then_wrapped_history_on_real_vte
    block_view::blocks::tests::a_precomputed_card_does_not_rewalk_its_transcript
    block_view::blocks::tests::lifecycle_chip_and_quick_actions_expose_truthful_status
    block_view::scroll::tests::widget_pool_releases_heavy_children_and_stale_controllers
    block_view::tests::entering_alt_screen_ends_the_block_selection_it_hides
    block_view::css::tests::the_generated_stylesheet_parses_without_error
    font::tests::a_real_vte_is_given_the_icon_family_behind_the_configured_one
    block_view::onboarding::tests::block_onboarding_overlay_is_non_measuring_and_non_targetable
    block_view::tests::a_stranded_focus_mount_declines_only_what_the_focused_widget_owns
    block_view::blocks::tests::revealing_a_cards_actions_does_not_move_its_metadata
    block_view::blocks::tests::the_selection_hint_sits_on_the_spacers_left
    block_view::tests::a_refusal_flash_is_visible_exposed_and_restores_only_the_latest_status
    block_view::tests::late_inline_notice_adopts_the_panes_current_density
    block_view::tests::dock_mount_refuses_a_widget_another_region_owns
    block_view::tests::unified_search_capture_real_vte_uses_half_open_column_boundary
    block_view::unified_chrome::tests::real_vte_osc8_row_probe_and_rewrap_smoke
    block_view::unified_chrome::tests::real_vte_non_bottom_anchor_calibrates_and_wide_badge_probe_fails_closed
    block_view::unified_images::tests::real_vte_keeps_nonzero_marker_column_through_narrow_wide_rewrap
)

export RUST_TEST_THREADS=1
export GDK_BACKEND=x11
export GSK_RENDERER=cairo
export LIBGL_ALWAYS_SOFTWARE=1
export NO_AT_BRIDGE=1
export GTK_A11Y=none

listed_tests="$(cargo test --lib --all-features --locked -- --ignored --list)"
for test_name in "${tests[@]}"; do
    if ! grep -Fqx -- "${test_name}: test" <<<"${listed_tests}"; then
        printf 'Error: GTK display regression is not registered: %s\n' "${test_name}" >&2
        exit 1
    fi
done

for test_name in "${tests[@]}"; do
    printf 'GTK display regression: %s\n' "${test_name}"
    dbus-run-session "${dbus_run_args[@]}" -- \
        xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' \
        cargo test --lib --all-features --locked "${test_name}" -- \
        --ignored --exact --nocapture
done

printf 'GTK display regressions passed: %d/%d.\n' "${#tests[@]}" "${#tests[@]}"
