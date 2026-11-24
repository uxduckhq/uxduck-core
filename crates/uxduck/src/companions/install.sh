# UXDuck's Install, Update and Remove on a companion computer (S297,
# ADR-0061 B5, B9). Runs as root in a transient systemd unit, so a dropped
# SSH link does not stop it half way. It reads only the folder it is given:
# action, version, sha256, build.tar.gz, unit, polkit, groups and replaces,
# each checked by the ground station before it was sent. It says what it
# does as "@step <title>", then that step's output, "@fail <why>" when the
# step failed, and last "@end done", "@end rolled-back" or "@end failed".
set -u
d=$1
action=$(cat "$d/action")
LINK=/usr/local/bin/uxduck
VERSIONS=/opt/uxduck
UNIT=uxduck-companion.service
UNITS=/etc/systemd/system
RULES=/etc/polkit-1/rules.d
RULE=$RULES/50-uxduck.rules
STATE=/var/lib/uxduck
USER_NAME=uxduck
# What UXDuck wrote, one entry a line, so Remove deletes that and nothing
# else, and what it replaced, so Remove puts that back: "replaced <unit>
# <was enabled> <was active>" and "kept $LINK".
WRITTEN=$VERSIONS/written
# How to put back the files this run changed, one command a line, run last
# first, then which services to turn on and start again once they are back.
UNDO=$d/undo
AFTER=$d/after
REPLACED=$d/replaced
: > "$UNDO"
: > "$AFTER"
: > "$REPLACED"

step() { echo "@step $1"; }
fail() { echo "@fail $1"; }
undo() { printf '%s\n' "$1" >> "$UNDO"; }
after() { printf '%s\n' "$1" >> "$AFTER"; }

put_back() {
    tac "$UNDO" | while IFS= read -r command; do
        sh -c "$command" 2>&1
    done
    systemctl daemon-reload 2>&1
    while IFS= read -r command; do
        sh -c "$command" 2>&1
    done < "$AFTER"
}

remove() {
    step "Stop and turn off $UNIT, and delete its file"
    systemctl disable --now "$UNIT" 2>&1
    rm -f "$UNITS/$UNIT"
    systemctl daemon-reload 2>&1
    step "Delete the polkit rule $RULE"
    rm -f "$RULE"
    step "Delete $LINK and every version under $VERSIONS"
    case "$(readlink "$LINK" 2>/dev/null)" in
        "$VERSIONS"/*) rm -f "$LINK" ;;
    esac
    owner=""
    made=no
    if [ -f "$WRITTEN" ]; then
        owner=$(sed -n 's/^chowned //p' "$WRITTEN" | tail -n 1)
        grep -qx "user $USER_NAME" "$WRITTEN" && made=yes
        grep -e '^replaced ' -e '^kept ' "$WRITTEN" > "$REPLACED"
    fi
    rm -rf "$VERSIONS"
    if [ -s "$REPLACED" ]; then
        step "Put back what UXDuck replaced"
        if grep -qx "kept $LINK" "$REPLACED"; then
            if [ -e "$LINK.before-uxduck" ] && [ ! -e "$LINK" ]; then
                mv -f "$LINK.before-uxduck" "$LINK" && echo "Put back the uxduck that was at $LINK."
            else
                echo "$LINK.before-uxduck is not there to put back."
            fi
        fi
        grep '^replaced ' "$REPLACED" | while read -r _ unit _ _; do
            if [ -f "$UNITS/$unit.before-uxduck" ]; then
                mv -f "$UNITS/$unit.before-uxduck" "$UNITS/$unit" && echo "Put back $UNITS/$unit."
                if [ -d "$UNITS/$unit.d.before-uxduck" ] && [ ! -e "$UNITS/$unit.d" ]; then
                    mv "$UNITS/$unit.d.before-uxduck" "$UNITS/$unit.d" && echo "Put back its drop-ins."
                fi
            else
                echo "$UNITS/$unit.before-uxduck is not there to put back."
            fi
        done
        systemctl daemon-reload 2>&1
    fi
    step "Give $STATE back to its owner if UXDuck gave it to $USER_NAME"
    if [ -n "$owner" ] && [ -d "$STATE" ]; then
        chown -R "$owner" "$STATE" 2>&1 && echo "Gave $STATE back to $owner."
    else
        echo "UXDuck did not change who owns it."
    fi
    step "Delete the system user $USER_NAME if UXDuck made it"
    if [ "$made" = yes ] && id "$USER_NAME" >/dev/null 2>&1; then
        userdel "$USER_NAME" 2>&1 || { fail "userdel could not delete $USER_NAME."; echo "@end failed"; exit 0; }
        echo "Deleted $USER_NAME."
    else
        echo "UXDuck did not make it, so it stays."
    fi
    if grep -q '^replaced ' "$REPLACED"; then
        step "Start what UXDuck replaced as it was"
        down=""
        while read -r _ unit enabled active; do
            [ -f "$UNITS/$unit" ] || continue
            [ "$enabled" = yes ] && systemctl enable "$unit" 2>&1
            if [ "$active" = yes ]; then
                systemctl reset-failed "$unit" 2>/dev/null
                systemctl start "$unit" 2>&1
                sleep 3
                if systemctl is-active --quiet "$unit"; then
                    echo "$unit is running."
                else
                    journalctl -u "$unit" -n 20 --no-pager -o cat 2>&1
                    down="$down $unit"
                fi
            else
                echo "$unit was not running before, so it stays stopped."
            fi
        done < "$REPLACED"
        if [ -n "$down" ]; then
            fail "UXDuck is removed and$down is back, but it did not start. Check it on the computer."
            echo "@end failed"
            return
        fi
    fi
    echo "@end done"
}

install_it() {
    version=$(cat "$d/version")
    sha=$(cat "$d/sha256")
    dir=$VERSIONS/$version
    step "Check the copied build against the release's SHA-256"
    if ! echo "$sha  $d/build.tar.gz" | sha256sum -c - 2>&1; then
        fail "The copy is not the build the release signed. Nothing was changed."
        echo "@end failed"
        return
    fi
    mkdir -p "$d/new" && tar -xzf "$d/build.tar.gz" -C "$d/new" 2>&1
    said=$("$d/new/uxduck" --version 2>&1 | head -n 1)
    echo "$said"
    if [ "$said" != "uxduck $version" ]; then
        fail "UXDuck $version does not run on this computer. Nothing was changed."
        echo "@end failed"
        return
    fi

    step "Make the system user $USER_NAME"
    mkdir -p "$VERSIONS"
    if id "$USER_NAME" >/dev/null 2>&1; then
        echo "$USER_NAME is there already."
    else
        if ! useradd --system --user-group --home-dir "$STATE" --no-create-home --shell /usr/sbin/nologin "$USER_NAME" 2>&1; then
            fail "useradd could not make $USER_NAME."
            put_back
            echo "@end failed"
            return
        fi
        undo "userdel $USER_NAME"
        made_user=yes
        echo "Made $USER_NAME."
    fi
    while IFS= read -r group; do
        [ -n "$group" ] || continue
        if getent group "$group" >/dev/null 2>&1; then
            usermod -aG "$group" "$USER_NAME" 2>&1 && echo "Added it to $group."
        else
            echo "This computer has no $group group."
        fi
    done < "$d/groups"

    step "Put UXDuck $version in $dir, with $LINK pointing at it"
    if [ ! -d "$dir" ]; then
        undo "rm -rf $dir"
    fi
    mkdir -p "$dir" && cp "$d/new/uxduck" "$dir/uxduck.new" && chmod 0755 "$dir/uxduck.new" && mv -f "$dir/uxduck.new" "$dir/uxduck" || {
        fail "Could not write $dir."
        put_back
        echo "@end failed"
        return
    }
    before=""
    if [ -L "$LINK" ]; then
        before=$(readlink "$LINK")
        undo "ln -sfn $before $LINK"
    elif [ -e "$LINK" ]; then
        mv -f "$LINK" "$LINK.before-uxduck"
        undo "mv -f $LINK.before-uxduck $LINK"
        echo "kept $LINK" >> "$REPLACED"
        echo "Kept the uxduck that was there as $LINK.before-uxduck."
    else
        undo "rm -f $LINK"
    fi
    ln -sfn "$dir/uxduck" "$LINK"

    while IFS= read -r unit; do
        [ -n "$unit" ] || continue
        step "Replace the hand-made $unit"
        was_active=no
        was_enabled=no
        systemctl is-active --quiet "$unit" && was_active=yes
        systemctl is-enabled --quiet "$unit" 2>/dev/null && was_enabled=yes
        systemctl disable --now "$unit" 2>&1
        mv -f "$UNITS/$unit" "$UNITS/$unit.before-uxduck"
        undo "mv -f $UNITS/$unit.before-uxduck $UNITS/$unit"
        echo "Kept its file as $UNITS/$unit.before-uxduck."
        if [ -d "$UNITS/$unit.d" ]; then
            rm -rf "$UNITS/$unit.d.before-uxduck"
            mv "$UNITS/$unit.d" "$UNITS/$unit.d.before-uxduck"
            undo "mv $UNITS/$unit.d.before-uxduck $UNITS/$unit.d"
            echo "Kept its drop-ins as $UNITS/$unit.d.before-uxduck."
        fi
        [ "$was_enabled" = yes ] && after "systemctl enable $unit"
        [ "$was_active" = yes ] && after "systemctl reset-failed $unit; systemctl start $unit"
        printf 'replaced %s %s %s\n' "$unit" "$was_enabled" "$was_active" >> "$REPLACED"
    done < "$d/replaces"

    step "Write $UNIT"
    if [ -f "$UNITS/$UNIT" ]; then
        cp -p "$UNITS/$UNIT" "$d/unit.before"
        undo "cp -p $d/unit.before $UNITS/$UNIT"
    else
        undo "rm -f $UNITS/$UNIT"
    fi
    if [ -d "$UNITS/$UNIT.d" ]; then
        rm -rf "$UNITS/$UNIT.d.before-uxduck"
        mv "$UNITS/$UNIT.d" "$UNITS/$UNIT.d.before-uxduck"
        undo "mv $UNITS/$UNIT.d.before-uxduck $UNITS/$UNIT.d"
        echo "Kept its drop-ins as $UNITS/$UNIT.d.before-uxduck."
    fi
    if systemctl is-enabled --quiet "$UNIT" 2>/dev/null; then
        after "systemctl enable $UNIT"
    fi
    if systemctl is-active --quiet "$UNIT"; then
        after "systemctl reset-failed $UNIT; systemctl restart $UNIT"
    fi
    cp "$d/unit" "$UNITS/$UNIT" && chmod 0644 "$UNITS/$UNIT"
    cat "$UNITS/$UNIT"

    step "Write the polkit rule $RULE"
    if [ -d "$RULES" ]; then
        if [ -f "$RULE" ]; then
            cp -p "$RULE" "$d/rule.before"
            undo "cp -p $d/rule.before $RULE"
        else
            undo "rm -f $RULE"
        fi
        cp "$d/polkit" "$RULE" && chmod 0644 "$RULE"
        echo "Wrote it."
    else
        echo "This computer has no polkit rules folder, so there is no rule to write."
    fi

    step "Give $STATE to $USER_NAME"
    owner=""
    if [ -d "$STATE" ]; then
        if [ "$(stat -c %U "$STATE")" = "$USER_NAME" ]; then
            echo "It is $USER_NAME's already."
        else
            owner=$(stat -c %u:%g "$STATE")
            chown -R "$USER_NAME:$USER_NAME" "$STATE" 2>&1
            undo "chown -R $owner $STATE"
            echo "Gave it to $USER_NAME."
        fi
    else
        echo "This computer has no $STATE."
    fi

    step "Start it, and put back what was there if it is not running UXDuck $version within 30 s"
    systemctl daemon-reload 2>&1
    systemctl enable "$UNIT" 2>&1
    undo "systemctl disable $UNIT"
    systemctl restart "$UNIT" 2>&1
    running=no
    waited=0
    while [ "$waited" -lt 30 ]; do
        sleep 1
        waited=$((waited + 1))
        pid=$(systemctl show -p MainPID --value "$UNIT")
        if systemctl is-active --quiet "$UNIT" && [ "$pid" != 0 ] && [ "$(readlink "/proc/$pid/exe")" = "$dir/uxduck" ]; then
            restarts=$(systemctl show -p NRestarts --value "$UNIT")
            sleep 3
            if systemctl is-active --quiet "$UNIT" && [ "$(systemctl show -p NRestarts --value "$UNIT")" = "$restarts" ] && [ "$(systemctl show -p MainPID --value "$UNIT")" = "$pid" ]; then
                running=yes
                break
            fi
        fi
    done
    if [ "$running" != yes ]; then
        journalctl -u "$UNIT" -n 20 --no-pager -o cat 2>&1
        fail "UXDuck $version was not running within 30 s."
        step "Put back what was there"
        systemctl stop "$UNIT" 2>&1
        systemctl reset-failed "$UNIT" 2>/dev/null
        put_back
        echo "Put back what was there."
        echo "@end rolled-back"
        return
    fi
    echo "Running UXDuck $version as $USER_NAME, process $pid."

    old=""
    for kept in "$VERSIONS"/*/; do
        [ -d "$kept" ] || continue
        name=$(basename "$kept")
        [ "$name" = "$version" ] && continue
        [ -n "$before" ] && [ "$before" = "$VERSIONS/$name/uxduck" ] && continue
        old="$old $name"
    done
    if [ -n "$old" ]; then
        step "Delete what is neither the new version nor the one to go back to"
        for name in $old; do
            rm -rf "${VERSIONS:?}/$name" && echo "Deleted $name."
        done
    fi

    {
        echo "unit $UNITS/$UNIT"
        [ -d "$RULES" ] && echo "rule $RULE"
        echo "link $LINK"
        if [ "${made_user:-no}" = yes ]; then
            echo "user $USER_NAME"
        elif [ -f "$WRITTEN" ] && grep -qx "user $USER_NAME" "$WRITTEN"; then
            echo "user $USER_NAME"
        fi
        if [ -n "$owner" ]; then
            echo "chowned $owner"
        elif [ -f "$WRITTEN" ]; then
            grep '^chowned ' "$WRITTEN"
        fi
        if [ -f "$WRITTEN" ]; then
            grep -e '^replaced ' -e '^kept ' "$WRITTEN"
        fi
        cat "$REPLACED"
    } > "$WRITTEN.new"
    mv -f "$WRITTEN.new" "$WRITTEN"
    echo "@end done"
}

case "$action" in
    remove) remove ;;
    install) install_it ;;
    *) echo "@end failed" ;;
esac
