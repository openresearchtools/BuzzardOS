#!/usr/bin/gjs
// SPDX-License-Identifier: AGPL-3.0-or-later

'use strict';

imports.gi.versions.Gvc = '1.0';
imports.gi.versions.Atspi = '2.0';

const {Atspi, GLib, Gvc} = imports.gi;
const System = imports.system;

const applicationId = ARGV[0] ?? 'org.openresearchtools.WildBuzzard';
const skippedApplicationIds = new Set([
    'org.gnome.VolumeControl',
    'org.PulseAudio.pavucontrol',
]);

const control = new Gvc.MixerControl({
    name: 'Wild Buzzard GNOME microphone acceptance probe',
});
const loop = new GLib.MainLoop(null, false);
let result = null;
let timeoutId = 0;

function childAt(accessible, index) {
    try {
        return accessible.get_child_at_index(index);
    } catch (_error) {
        return null;
    }
}

function children(accessible) {
    const result = [];
    let count;
    try {
        count = accessible.get_child_count();
    } catch (_error) {
        return result;
    }
    for (let index = 0; index < count; index++) {
        const child = childAt(accessible, index);
        if (child !== null)
            result.push(child);
    }
    return result;
}

function state(accessible, type) {
    try {
        return accessible.get_state_set().contains(type);
    } catch (_error) {
        return false;
    }
}

function extents(accessible) {
    try {
        const value = accessible.get_extents(Atspi.CoordType.SCREEN);
        if (value.width <= 0 || value.height <= 0 ||
            value.x === -2147483648 || value.y === -2147483648)
            return null;
        return {
            x: value.x,
            y: value.y,
            width: value.width,
            height: value.height,
        };
    } catch (_error) {
        return null;
    }
}

function role(accessible) {
    try {
        return accessible.get_role();
    } catch (_error) {
        return Atspi.Role.INVALID;
    }
}

function walk(accessible, maximumDepth) {
    const result = [];
    const pending = [{accessible, depth: 0}];
    while (pending.length > 0) {
        const {accessible: current, depth} = pending.pop();
        result.push(current);
        if (depth < maximumDepth) {
            pending.push(...children(current).map(child => ({
                accessible: child,
                depth: depth + 1,
            })));
        }
    }
    return result;
}

function visible(accessible) {
    return state(accessible, Atspi.StateType.SHOWING) &&
        state(accessible, Atspi.StateType.VISIBLE);
}

function inspectShellIndicator() {
    try {
        Atspi.init();
        const desktop = Atspi.get_desktop(0);
        const shell = children(desktop).find(application => {
            try {
                return application.get_name() === 'gnome-shell';
            } catch (_error) {
                return false;
            }
        });
        if (shell === undefined)
            return {available: false, reason: 'gnome-shell is absent from host AT-SPI'};

        // GNOME Shell exposes its top panel as a wide, shallow panel wrapping
        // one three-part panel (left, centre, right). Select by that structure
        // and aspect ratio, not by translated labels, screen size, or position.
        const panelCandidates = walk(shell, 4).flatMap(candidate => {
            if (role(candidate) !== Atspi.Role.PANEL || !visible(candidate))
                return [];
            const outerExtents = extents(candidate);
            const inner = childAt(candidate, 0);
            const innerExtents = inner === null ? null : extents(inner);
            if (outerExtents === null || innerExtents === null ||
                children(candidate).length !== 1 ||
                role(inner) !== Atspi.Role.PANEL || children(inner).length !== 3 ||
                outerExtents.width !== innerExtents.width ||
                outerExtents.height !== innerExtents.height ||
                outerExtents.width <= outerExtents.height)
                return [];
            return [{outer: candidate, inner, extents: outerExtents}];
        });
        panelCandidates.sort((left, right) =>
            right.extents.width / right.extents.height -
            left.extents.width / left.extents.height);
        const topPanel = panelCandidates[0];
        if (topPanel === undefined)
            return {available: false, reason: 'GNOME top-panel structure is absent from AT-SPI'};

        // The system-status menu is the rightmost visible menu in the panel.
        // Its first child is the long-lived indicator box whose pre-created
        // children toggle SHOWING/VISIBLE as privacy indicators come and go.
        const menuCandidates = walk(topPanel.inner, 4).flatMap(candidate => {
            const candidateExtents = extents(candidate);
            if (role(candidate) !== Atspi.Role.MENU || !visible(candidate) ||
                candidateExtents === null)
                return [];
            return [{accessible: candidate, extents: candidateExtents}];
        });
        menuCandidates.sort((left, right) => {
            const rightEdgeDifference =
                right.extents.x + right.extents.width -
                (left.extents.x + left.extents.width);
            return rightEdgeDifference !== 0
                ? rightEdgeDifference
                : right.extents.width - left.extents.width;
        });
        const systemMenu = menuCandidates[0];
        const indicatorBox = systemMenu === undefined
            ? null
            : childAt(systemMenu.accessible, 0);
        if (indicatorBox === null)
            return {available: false, reason: 'GNOME system-status indicator box is absent from AT-SPI'};

        const indicatorChildren = children(indicatorBox).map((child, index) => {
            const childExtents = extents(child);
            return {
                index,
                showing: state(child, Atspi.StateType.SHOWING),
                visible: state(child, Atspi.StateType.VISIBLE),
                enabled: state(child, Atspi.StateType.ENABLED),
                width: childExtents?.width ?? 0,
                height: childExtents?.height ?? 0,
            };
        });
        return {
            available: true,
            discovery: 'top-panel-structure/rightmost-visible-menu/direct-child-state',
            top_panel: {
                width: topPanel.extents.width,
                height: topPanel.extents.height,
            },
            system_menu: {
                width: systemMenu.extents.width,
                height: systemMenu.extents.height,
            },
            indicator_children: indicatorChildren,
            showing_child_indices: indicatorChildren
                .filter(child => child.showing && child.visible)
                .map(child => child.index),
        };
    } catch (error) {
        return {available: false, reason: String(error)};
    }
}

function streamRecord(stream) {
    return {
        id: stream.get_id(),
        application_id: stream.get_application_id() ?? null,
        name: stream.get_name() ?? null,
        description: stream.get_description() ?? null,
    };
}

function inspectReadyControl() {
    const sourceOutputs = control.get_source_outputs();
    const defaultSource = control.get_default_source();
    const matchingOutputs = sourceOutputs.filter(
        output => output.get_application_id() === applicationId);
    const shellVisibleOutputs = sourceOutputs.filter(
        output => !skippedApplicationIds.has(output.get_application_id()));
    const defaultSourceMuted = defaultSource?.get_is_muted() ?? true;

    // GNOME Shell's InputStreamSlider uses get_source_outputs(), excluding
    // only its own volume-control stream and pavucontrol. Its privacy style is
    // active only while the default source exists and is not muted.
    result = {
        ready: true,
        application_id: applicationId,
        wild_buzzard_tracked: matchingOutputs.length > 0,
        wild_buzzard_privacy_indicator_expected:
            matchingOutputs.length > 0 && defaultSource !== null && !defaultSourceMuted,
        gnome_input_visible:
            defaultSource !== null && shellVisibleOutputs.length > 0,
        gnome_privacy_indicator_expected:
            defaultSource !== null && shellVisibleOutputs.length > 0 && !defaultSourceMuted,
        default_source: defaultSource === null
            ? null
            : {
                id: defaultSource.get_id(),
                name: defaultSource.get_name() ?? null,
                description: defaultSource.get_description() ?? null,
                muted: defaultSourceMuted,
            },
        matching_source_outputs: matchingOutputs.map(streamRecord),
        source_outputs: sourceOutputs.map(streamRecord),
        shell_indicator: inspectShellIndicator(),
    };
    loop.quit();
}

control.connect('state-changed', () => {
    if (control.get_state() === Gvc.MixerControlState.READY)
        inspectReadyControl();
});

try {
    control.open();
    if (control.get_state() === Gvc.MixerControlState.READY) {
        inspectReadyControl();
    } else {
        timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 5000, () => {
            timeoutId = 0;
            result = {
                ready: false,
                application_id: applicationId,
                error: 'timed out waiting for Gvc.MixerControl',
                mixer_state: control.get_state(),
            };
            loop.quit();
            return GLib.SOURCE_REMOVE;
        });
        loop.run();
    }
} catch (error) {
    result = {
        ready: false,
        application_id: applicationId,
        error: String(error),
    };
}

if (timeoutId !== 0)
    GLib.source_remove(timeoutId);
control.close();
print(JSON.stringify(result));

if (!result?.ready)
    System.exit(2);
