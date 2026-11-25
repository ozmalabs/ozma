/*
 * Invalid-but-readable MagicMirror fixture. The config object contains an
 * expression call, so the bounded reader must refuse it rather than evaluate
 * it. The call writes a sentinel file if anything ever executes this source.
 */
var config = {
  modules: [],
  touched: require('fs').writeFileSync(
    '/tmp/ozma_magicmirror_invalid_executed',
    'executed'
  ),
};
