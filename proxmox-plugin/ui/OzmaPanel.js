/*
 * Ozma Proxmox Plugin — Web UI Extension
 *
 * Adds an "Ozma" tab to each VM's management page with:
 *   - Live display preview (MJPEG from the display service)
 *   - Interactive KVM control (keyboard + mouse)
 *   - VM profile selection (gaming/workstation/server/media)
 *   - Display layout configuration
 *   - Audio channel assignment
 *   - Agent status
 *   - Quick actions (screenshot, RPA, OCR)
 */

Ext.define('PVE.qemu.OzmaPanel', {
    extend: 'Ext.panel.Panel',
    alias: 'widget.pveQemuOzmaPanel',

    title: 'Ozma',
    iconCls: 'fa fa-desktop',
    layout: 'border',

    initComponent: function() {
        var me = this;
        var vmid = me.pveSelNode.data.vmid;
        var nodename = me.pveSelNode.data.node;

        // Display preview panel
        var displayPanel = Ext.create('Ext.panel.Panel', {
            region: 'center',
            title: 'Live Display',
            html: '<div id="ozma-display-' + vmid + '" style="width:100%;height:100%;background:#000;display:flex;align-items:center;justify-content:center;">' +
                  '<img id="ozma-preview-' + vmid + '" style="max-width:100%;max-height:100%;cursor:crosshair;" />' +
                  '</div>',
            tools: [{
                type: 'maximize',
                tooltip: 'Interactive Control',
                handler: function() {
                    me.openInteractiveControl(vmid);
                }
            }],
            listeners: {
                afterrender: function() {
                    me.startPreview(vmid);
                }
            }
        });

        // Status panel
        var statusPanel = Ext.create('Ext.panel.Panel', {
            region: 'east',
            width: 300,
            split: true,
            title: 'Status',
            layout: 'vbox',
            defaults: { width: '100%', padding: 5 },
            items: [{
                xtype: 'displayfield',
                fieldLabel: 'Display',
                itemId: 'displayType',
                value: 'Detecting...'
            }, {
                xtype: 'displayfield',
                fieldLabel: 'Resolution',
                itemId: 'resolution',
                value: '-'
            }, {
                xtype: 'displayfield',
                fieldLabel: 'Agent',
                itemId: 'agentStatus',
                value: 'Not installed'
            }, {
                xtype: 'displayfield',
                fieldLabel: 'Profile',
                itemId: 'profile',
                value: '-'
            }, {
                xtype: 'component',
                html: '<hr style="border-color:#444">'
            }, {
                xtype: 'button',
                text: 'Apply Profile',
                iconCls: 'fa fa-magic',
                handler: function() {
                    me.showProfileDialog(vmid);
                }
            }, {
                xtype: 'button',
                text: 'Screenshot',
                iconCls: 'fa fa-camera',
                margin: '5 0 0 0',
                handler: function() {
                    me.takeScreenshot(vmid);
                }
            }, {
                xtype: 'button',
                text: 'Install Agent',
                iconCls: 'fa fa-download',
                margin: '5 0 0 0',
                handler: function() {
                    Ext.Msg.confirm('Install Agent',
                        'Install the ozma agent inside VM ' + vmid + '?<br><br>' +
                        'This will:<br>' +
                        '• Detect the guest OS (Windows/Linux)<br>' +
                        '• Install Python if needed<br>' +
                        '• Install and start the ozma agent<br><br>' +
                        'Requires: QEMU Guest Agent running inside the VM.',
                        function(btn) {
                            if (btn !== 'yes') return;
                            Proxmox.Utils.API2Request({
                                url: '/api2/json/ozma/agent/install',
                                method: 'POST',
                                params: { vmid: vmid },
                                waitMsgTarget: me,
                                success: function(response) {
                                    var data = response.result.data;
                                    if (data.ok) {
                                        Ext.Msg.alert('Success',
                                            'Agent installation started via ' + (data.method || 'auto') + '.<br>' +
                                            (data.message || ''));
                                    } else {
                                        Ext.Msg.alert('Failed', data.error || 'Unknown error');
                                    }
                                },
                                failure: function(response) {
                                    Ext.Msg.alert('Error', response.htmlStatus);
                                }
                            });
                        }
                    );
                }
            }, {
                xtype: 'button',
                text: 'Open Dashboard',
                iconCls: 'fa fa-external-link',
                margin: '5 0 0 0',
                handler: function() {
                    var url = '/api2/json/ozma/status';
                    Proxmox.Utils.API2Request({
                        url: url,
                        success: function(response) {
                            var controller = response.result.data.controller;
                            window.open(controller, '_blank');
                        }
                    });
                }
            }]
        });

        me.items = [displayPanel, statusPanel];
        me.callParent();

        // Poll display status
        me.statusTimer = setInterval(function() {
            me.updateStatus(vmid);
        }, 5000);

        me.on('destroy', function() {
            clearInterval(me.statusTimer);
        });
    },

    startPreview: function(vmid) {
        var img = document.getElementById('ozma-preview-' + vmid);
        if (img) {
            // API port = 7390 + vmid
            var port = 7390 + parseInt(vmid);
            img.src = '/api2/json/ozma/display/' + vmid + '/snapshot?' + Date.now();

            // Refresh every 2 seconds
            setInterval(function() {
                img.src = '/api2/json/ozma/display/' + vmid + '/snapshot?' + Date.now();
            }, 2000);
        }
    },

    updateStatus: function(vmid) {
        var me = this;
        var port = 7390 + parseInt(vmid);

        Proxmox.Utils.API2Request({
            url: '/api2/json/ozma/status',
            success: function(response) {
                var data = response.result.data;
                var panel = me.down('[itemId=displayType]');
                if (panel) panel.setValue(data.display_backend || 'unknown');
            },
            failure: function() {}
        });
    },

    openInteractiveControl: function(vmid) {
        // Open fullscreen interactive KVM control
        // Similar to the dashboard's Control button
        var port = 7390 + parseInt(vmid);
        var win = window.open('about:blank', 'ozma-control-' + vmid,
            'width=1024,height=768,menubar=no,toolbar=no');
        win.document.write(
            '<html><head><title>Ozma Control - VM ' + vmid + '</title>' +
            '<style>body{margin:0;background:#000;display:flex;align-items:center;justify-content:center;height:100vh}' +
            'img{max-width:100%;max-height:100%;cursor:crosshair}</style></head>' +
            '<body><img id="screen" />' +
            '<script>' +
            'var img=document.getElementById("screen");' +
            'setInterval(function(){img.src="/api2/json/ozma/display/' + vmid + '/snapshot?"+Date.now()},100);' +
            '</script></body></html>'
        );
    },

    showProfileDialog: function(vmid) {
        Ext.create('Ext.window.Window', {
            title: 'Apply Ozma Profile - VM ' + vmid,
            width: 400,
            modal: true,
            items: [{
                xtype: 'form',
                bodyPadding: 10,
                items: [{
                    xtype: 'combo',
                    fieldLabel: 'Profile',
                    name: 'profile',
                    store: ['gaming', 'workstation', 'server', 'media'],
                    value: 'workstation',
                    editable: false
                }, {
                    xtype: 'textfield',
                    fieldLabel: 'GPU (PCI)',
                    name: 'gpu',
                    emptyText: 'e.g. 0000:01:00.0 (gaming only)'
                }, {
                    xtype: 'numberfield',
                    fieldLabel: 'Cores',
                    name: 'cores',
                    minValue: 1,
                    maxValue: 128,
                    value: 4
                }, {
                    xtype: 'numberfield',
                    fieldLabel: 'Memory (MB)',
                    name: 'memory',
                    minValue: 512,
                    step: 1024,
                    value: 8192
                }]
            }],
            buttons: [{
                text: 'Apply',
                handler: function() {
                    var form = this.up('window').down('form');
                    var values = form.getValues();
                    values.vmid = vmid;

                    Proxmox.Utils.API2Request({
                        url: '/api2/json/ozma/profiles/apply',
                        method: 'POST',
                        params: values,
                        success: function() {
                            Ext.Msg.alert('Success', 'Profile applied. Restart VM to take effect.');
                        },
                        failure: function(response) {
                            Ext.Msg.alert('Error', response.htmlStatus);
                        }
                    });
                    this.up('window').close();
                }
            }, {
                text: 'Cancel',
                handler: function() { this.up('window').close(); }
            }]
        }).show();
    },

    takeScreenshot: function(vmid) {
        var port = 7390 + parseInt(vmid);
        window.open('/api2/json/ozma/display/' + vmid + '/snapshot', '_blank');
    }
});

// Register the panel with Proxmox's VM view
// This hooks into PVE.qemu.Config to add our tab
Ext.define('PVE.qemu.OzmaTabOverride', {
    override: 'PVE.qemu.Config',

    initComponent: function() {
        this.callParent();

        // Add Ozma tab after the existing tabs
        var me = this;
        me.insert(me.items.length - 1, {
            xtype: 'pveQemuOzmaPanel',
            title: 'Ozma',
            iconCls: 'fa fa-desktop',
            itemId: 'ozma',
            pveSelNode: me.pveSelNode
        });
    }
});
