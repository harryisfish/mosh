use std::collections::BTreeSet;

use anyhow::{Result, ensure};

use crate::diagnostics::{self, Context, Stage};
use crate::model::{Frame, MAX_RESOURCES, RowPatch, Screen};

pub fn frame(
    epoch: u64,
    revision: u64,
    echo_through: u64,
    screen: &Screen,
    base: Option<(u64, &Screen)>,
) -> Result<Frame> {
    let _operation = diagnostics::operation(
        Stage::FrameDiff,
        Context {
            epoch,
            revision,
            ..Default::default()
        },
    );
    screen.size.validate()?;
    ensure!(
        screen.rows.len() == screen.size.rows as usize,
        "terminal row count mismatch"
    );
    let base = base.filter(|(_, b)| b.size == screen.size);
    let rows = screen
        .rows
        .iter()
        .enumerate()
        .filter_map(|(y, row)| {
            if base.is_some_and(|(_, b)| b.rows.get(y) == Some(row)) {
                None
            } else {
                Some(RowPatch {
                    y: y as u16,
                    row: row.clone(),
                })
            }
        })
        .collect();
    let referenced: BTreeSet<_> = screen
        .rows
        .iter()
        .flat_map(|row| row.cells.iter())
        .flat_map(|cell| cell.images.iter().map(|tile| tile.resource))
        .collect();
    let mut total = 0usize;
    let mut resources = Vec::with_capacity(referenced.len());
    for digest in referenced {
        let resource = screen
            .resources
            .get(&digest)
            .ok_or_else(|| anyhow::anyhow!("missing terminal image resource"))?;
        resource.validate()?;
        total += resource.rgba.len();
        ensure!(total <= MAX_RESOURCES, "live screen resources exceed limit");
        resources.push(resource.meta.clone());
    }
    Ok(Frame {
        epoch,
        revision,
        base: base.map(|(revision, _)| revision),
        echo_through,
        size: screen.size,
        rows,
        cursor: screen.cursor,
        title: screen.title.clone(),
        directory: screen.directory.clone(),
        markers: screen.markers.clone(),
        reverse_video: screen.reverse_video,
        prediction: screen.prediction,
        resources,
    })
}
