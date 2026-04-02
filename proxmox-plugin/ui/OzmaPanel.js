/*
 * Ozma Proxmox Plugin — Web UI Extension
 *
 * Adds to each VM's management page:
 *   1. "Ozma Console" button in the VM toolbar (WebRTC, <5ms latency)
 *   2. "Ozma" tab with live preview, status, profiles
 */

// ── Inject Ozma Console button into VM toolbar ──────────────────────────
Ext.define('PVE.qemu.OzmaInjector', {
    override: 'PVE.qemu.Config',

    initComponent: function() {
        var me = this;
        me.callParent();

        var vmid = me.pveSelNode.data.vmid;
        var vmname = me.pveSelNode.data.text || me.pveSelNode.data.name || ('VM ' + vmid);

        // Add Ozma Console button to the top toolbar
        var tbar = me.down('toolbar');
        if (tbar) {
            // Insert after the first few buttons (Start, Shutdown, etc.)
            tbar.add('-');
            tbar.add({
                xtype: 'button',
                text: 'Ozma Console',
                iconCls: 'fa fa-bolt',
                tooltip: 'Open Ozma WebRTC Console (low latency)',
                handler: function() {
                    var w = Math.min(screen.width - 100, 1920);
                    var h = Math.min(screen.height - 100, 1080);
                    var l = Math.round((screen.width - w) / 2);
                    var t = Math.round((screen.height - h) / 2);
                    window.open(
                        '/ozma/console/?vmid=' + vmid +
                        '&name=' + encodeURIComponent(vmname),
                        'ozma-console-' + vmid,
                        'width=' + w + ',height=' + h + ',left=' + l + ',top=' + t +
                        ',menubar=no,toolbar=no,location=no,status=no'
                    );
                }
            });
        }

        // Add Ozma tab to the VM panel
        me.addDocked({
            // nothing — we insert as a tab below
        });

        // Insert Ozma tab before the last item
        var tabIdx = me.items.length > 1 ? me.items.length - 1 : me.items.length;
        me.insert(tabIdx, {
            xtype: 'panel',
            title: 'Ozma',
            iconCls: 'fa fa-desktop',
            itemId: 'ozma',
            layout: 'border',
            items: [{
                // Display preview (center)
                region: 'center',
                xtype: 'panel',
                title: 'Live Display',
                bodyStyle: 'background:#111;display:flex;align-items:center;justify-content:center;cursor:pointer',
                html: '<img id="ozma-preview-' + vmid + '" style="max-width:100%;max-height:100%" />' +
                      '<div style="position:absolute;bottom:8px;left:8px;background:rgba(0,0,0,0.7);' +
                      'color:#e94560;padding:2px 8px;border-radius:3px;font-size:11px">' +
                      'Click to open Ozma Console</div>',
                listeners: {
                    afterrender: function(p) {
                        // Click to open console
                        p.getEl().on('click', function() {
                            var w = Math.min(screen.width - 100, 1920);
                            var h = Math.min(screen.height - 100, 1080);
                            window.open(
                                '/ozma/console/?vmid=' + vmid + '&name=' + encodeURIComponent(vmname),
                                'ozma-console-' + vmid,
                                'width=' + w + ',height=' + h + ',menubar=no,toolbar=no'
                            );
                        });

                        // Start snapshot preview polling
                        var img = document.getElementById('ozma-preview-' + vmid);
                        if (img) {
                            var refresh = function() {
                                var next = new Image();
                                next.onload = function() { img.src = next.src; };
                                next.src = '/ozma/display/snapshot?t=' + Date.now();
                            };
                            refresh();
                            var timer = setInterval(refresh, 2000);
                            p.on('destroy', function() { clearInterval(timer); });
                        }
                    }
                }
            }, {
                // Status panel (right)
                region: 'east',
                width: 260,
                split: true,
                xtype: 'panel',
                title: 'Status',
                bodyPadding: 8,
                defaults: { margin: '4 0' },
                items: [{
                    xtype: 'displayfield',
                    fieldLabel: 'Display',
                    itemId: 'ozma-display-type',
                    value: '<i>detecting...</i>'
                }, {
                    xtype: 'displayfield',
                    fieldLabel: 'Resolution',
                    itemId: 'ozma-resolution',
                    value: '-'
                }, {
                    xtype: 'displayfield',
                    fieldLabel: 'Frames',
                    itemId: 'ozma-frames',
                    value: '0'
                }, {
                    xtype: 'displayfield',
                    fieldLabel: 'Port',
                    value: String(7390 + parseInt(vmid))
                }, {
                    xtype: 'component',
                    html: '<hr style="border-color:#444">'
                }, {
                    xtype: 'button',
                    text: 'Open Ozma Console',
                    iconCls: 'fa fa-bolt',
                    width: '100%',
                    scale: 'medium',
                    handler: function() {
                        var w = Math.min(screen.width - 100, 1920);
                        var h = Math.min(screen.height - 100, 1080);
                        window.open(
                            '/ozma/console/?vmid=' + vmid + '&name=' + encodeURIComponent(vmname),
                            'ozma-console-' + vmid,
                            'width=' + w + ',height=' + h + ',menubar=no,toolbar=no'
                        );
                    }
                }, {
                    xtype: 'button',
                    text: 'Screenshot',
                    iconCls: 'fa fa-camera',
                    width: '100%',
                    margin: '4 0',
                    handler: function() {
                        window.open('/ozma/display/snapshot', '_blank');
                    }
                }],
                listeners: {
                    afterrender: function(p) {
                        // Poll display status
                        var timer = setInterval(function() {
                            fetch('/ozma/display/info')
                                .then(function(r) { return r.json(); })
                                .then(function(d) {
                                    var dt = p.down('[itemId=ozma-display-type]');
                                    if (dt) dt.setValue(d.type || 'unknown');
                                    var res = p.down('[itemId=ozma-resolution]');
                                    if (res && d.width) res.setValue(d.width + ' x ' + d.height);
                                    var fc = p.down('[itemId=ozma-frames]');
                                    if (fc) fc.setValue(String(d.frame_count || 0));
                                })
                                .catch(function() {
                                    var dt = p.down('[itemId=ozma-display-type]');
                                    if (dt) dt.setValue('<span style="color:red">offline</span>');
                                });
                        }, 3000);
                        p.on('destroy', function() { clearInterval(timer); });
                    }
                }
            }]
        });
    }
});
