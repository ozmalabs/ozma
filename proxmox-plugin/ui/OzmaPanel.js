/*
 * Ozma Proxmox Plugin — Web UI Extension
 *
 * Adds "Ozma Console" tab to each VM's sidebar — one click opens a
 * WebRTC console with <5ms latency, replacing PVE's noVNC.
 */

// Inject ozma icon CSS — simple hexagonal O
(function() {
    var style = document.createElement('style');
    style.textContent =
        ".fa.fa-ozma:before{content:url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16' width='14' height='14'%3E" +
        "%3Cpath d='M8 0.5L14.5 4.25V11.75L8 15.5L1.5 11.75V4.25Z' fill='none' stroke='white' stroke-width='3' stroke-linejoin='round'/%3E" +
        '%3C/svg%3E")}';
    document.head.appendChild(style);
})();

Ext.define('PVE.qemu.OzmaConsoleTab', {
    override: 'PVE.qemu.Config',

    initComponent: function() {
        var me = this;
        me.callParent();

        var vm = me.pveSelNode.data;
        var vmid = vm.vmid;
        var vmname = vm.name || vm.text || ('VM ' + vmid);
        var frameId = 'ozma-frame-' + vmid;

        me.insertNodes([{
            xtype: 'panel',
            title: 'Ozma Console',
            iconCls: 'fa fa-ozma',
            itemId: 'ozma-console',
            layout: 'fit',
            html: '<iframe id="' + frameId + '" ' +
                  'tabindex="0" ' +
                  'style="width:100%;height:100%;border:none" ' +
                  'src="/ozma/console/?node=' + encodeURIComponent(vmname) + '&api=/ozma&_=' + Date.now() +
                  '" allow="autoplay; clipboard-write; fullscreen" allowfullscreen></iframe>',
            listeners: {
                // Focus the iframe when the Ozma Console tab is activated
                activate: function() {
                    var frame = document.getElementById(frameId);
                    if (frame) {
                        // Short delay to let ExtJS finish layout
                        setTimeout(function() {
                            frame.focus();
                            // Also tell the iframe content to grab focus
                            try { frame.contentWindow.focus(); } catch(e) {}
                        }, 100);
                    }
                },
                afterrender: function() {
                    var frame = document.getElementById(frameId);
                    if (frame) {
                        // Focus iframe when clicked
                        frame.addEventListener('mouseenter', function() {
                            frame.focus();
                        });
                    }
                },
            },
        }]);
    },
});
