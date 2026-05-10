#!/usr/bin/env bash
# Visualize jterm4 session state

STATE_FILE="${HOME}/.config/jterm4/tabs.state"

if [ ! -f "$STATE_FILE" ]; then
    echo "❌ No state file found at $STATE_FILE"
    exit 1
fi

echo "📊 jterm4 Session State Visualization"
echo "======================================"
echo ""

# Parse current page
current_page=$(grep '^current_page=' "$STATE_FILE" | cut -d= -f2)
echo "📌 Current Page: ${current_page:-0}"
echo ""

# Parse tabs
tab_num=0
while IFS=$'\t' read -r line; do
    if [[ "$line" =~ ^tab= ]]; then
        tab_num=$((tab_num + 1))

        # Remove "tab=" prefix
        data="${line#tab=}"

        # Split by tab
        IFS=$'\t' read -r name layout_json <<< "$data"

        echo "📑 Tab $tab_num: $name"

        # Try to parse as JSON
        if echo "$layout_json" | jq . &> /dev/null 2>&1; then
            layout_type=$(echo "$layout_json" | jq -r '.type')

            case "$layout_type" in
                leaf)
                    dir=$(echo "$layout_json" | jq -r '.dir')
                    sid=$(echo "$layout_json" | jq -r '.sid')
                    cmds=$(echo "$layout_json" | jq -r '.cmds // "none"')

                    echo "   Type: Single pane"
                    echo "   Directory: $dir"
                    echo "   Session ID: $sid"
                    echo "   Commands: $cmds"
                    ;;

                split)
                    orientation=$(echo "$layout_json" | jq -r '.orientation')
                    position=$(echo "$layout_json" | jq -r '.position')

                    orientation_name=$([[ "$orientation" == "h" ]] && echo "Horizontal" || echo "Vertical")

                    echo "   Type: Split pane ($orientation_name)"
                    echo "   Position: $position"
                    echo "   Structure:"

                    # Show simplified tree
                    echo "$layout_json" | jq -C '.' | sed 's/^/      /'
                    ;;
            esac
        else
            # Legacy format or simple directory
            echo "   Format: Legacy"
            echo "   Data: $layout_json"
        fi

        echo ""
    fi
done < "$STATE_FILE"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Total tabs: $tab_num"
echo ""
echo "💡 Tip: Edit state file at: $STATE_FILE"
