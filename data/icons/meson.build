scalable_dir = 'hicolor' / 'scalable' / 'apps'

install_data(
  scalable_dir / '@0@.svg'.format(app_id),
  install_dir: datadir / 'icons' / scalable_dir,
)

symbolic_dir = 'hicolor' / 'symbolic' / 'apps'

install_data(
  symbolic_dir / 'org.gnome.Loupe-symbolic.svg',
  install_dir: datadir / 'icons' / symbolic_dir,
  rename: '@0@-symbolic.svg'.format(app_id),
)

actions_dir = 'hicolor' / 'scalable' / 'actions'

install_data(
  [
    actions_dir / 'loupe-tool-pencil-symbolic.svg',
    actions_dir / 'loupe-tool-brush-symbolic.svg',
    actions_dir / 'loupe-tool-eraser-symbolic.svg',
    actions_dir / 'loupe-tool-highlight-symbolic.svg',
    actions_dir / 'loupe-tool-line-symbolic.svg',
    actions_dir / 'loupe-tool-arrow-symbolic.svg',
    actions_dir / 'loupe-tool-rectangle-symbolic.svg',
    actions_dir / 'loupe-tool-round-rect-symbolic.svg',
    actions_dir / 'loupe-tool-oval-symbolic.svg',
    actions_dir / 'loupe-tool-text-symbolic.svg',
    actions_dir / 'loupe-tool-paint-symbolic.svg',
    actions_dir / 'loupe-tool-filters-symbolic.svg',
  ],
  install_dir: datadir / 'icons' / actions_dir,
)
