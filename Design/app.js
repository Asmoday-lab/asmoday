const { createApp, ref, computed, onMounted, nextTick } = Vue;

createApp({
    setup() {
        const currentView = ref('portal'); 
        const isSidebarCollapsed = ref(false);
        const isMobileMenuOpen = ref(false); 

        const navTo = (view) => {
            currentView.value = view;
            isMobileMenuOpen.value = false;
        };

        // ================= 主题系统 =================
        const themeMode = ref('dark'); 
        const toggleTheme = () => {
            themeMode.value = themeMode.value === 'dark' ? 'light' : 'dark';
            if (themeMode.value === 'light') document.documentElement.setAttribute('data-theme', 'light');
            else document.documentElement.removeAttribute('data-theme');
        };
        const themeText = computed(() => themeMode.value === 'dark' ? '深色' : '浅色');
        const themeIcon = computed(() => themeMode.value === 'dark' ? 'fa-solid fa-moon' : 'fa-solid fa-sun');

        // ================= 终端与粘性键 =================
        const terminalTabs = ref([{ id: 1, name: 'bash', cwd: '~' }]); 
        const activeTerminalId = ref(1);
        let termInstances = {};

        const showShortcuts = ref(true);
        const isCtrlActive = ref(false);
        const isAltActive = ref(false);

        const toggleStickyKey = (key) => {
            if (key === 'ctrl') { isCtrlActive.value = !isCtrlActive.value; isAltActive.value = false; }
            if (key === 'alt') { isAltActive.value = !isAltActive.value; isCtrlActive.value = false; }
        };

        const initXterm = (id, cwd) => {
            nextTick(() => {
                const el = document.getElementById(`xterm-${id}`);
                if (el && !termInstances[id]) {
                    const term = new Terminal({ theme: { background: '#1e1e1e' }, fontSize: 13 });
                    term.open(el);
                    term.write(`root@lumine:${cwd}# `);
                    
                    term.onData(e => {
                        let dataToSend = e;
                        if (isCtrlActive.value) {
                            const code = e.toUpperCase().charCodeAt(0);
                            if (code >= 64 && code <= 95) dataToSend = String.fromCharCode(code - 64);
                            isCtrlActive.value = false; 
                        } else if (isAltActive.value) {
                            dataToSend = '\x1b' + e;
                            isAltActive.value = false;
                        }

                        if (dataToSend === '\r') term.write(`\r\nroot@lumine:${cwd}# `);
                        else if (dataToSend === '\x03') term.write('^C\r\nroot@lumine:${cwd}# ');
                        else if (dataToSend === '\x1b') term.write('^[');
                        else term.write(dataToSend);
                    });
                    termInstances[id] = term;
                    term.focus();
                }
            });
        };

        const createTerminalTab = (cwd) => {
            const id = Date.now();
            terminalTabs.value.push({ id, name: `bash`, cwd });
            activeTerminalId.value = id;
            navTo('terminal');
            initXterm(id, cwd);
        };

        const closeTerminalTab = (id) => {
            const idx = terminalTabs.value.findIndex(t => t.id === id);
            if (idx !== -1) terminalTabs.value.splice(idx, 1);
            if (terminalTabs.value.length > 0) activeTerminalId.value = terminalTabs.value[0].id;
        };

        const sendTermKey = (charCode) => {
            if (!activeTerminalId.value || !termInstances[activeTerminalId.value]) return;
            termInstances[activeTerminalId.value].write(charCode);
            termInstances[activeTerminalId.value].focus();
        };

        // ================= 文件管理与编辑器 =================
        const currentPath = ref('/var/www');
        const directoryTree = ref([
            { id: 1, name: '根目录', path: '/', level: 0, icon: 'fa-solid fa-hard-drive' },
            { id: 2, name: 'www', path: '/var/www', level: 1, icon: 'fa-solid fa-folder-open text-theme-accent' },
        ]);
        const currentFiles = ref([
            { id: 101, name: '2025相册', isDir: true, type: 'folder', icon: 'fa-solid fa-folder text-yellow-500' },
            { id: 102, name: 'main.js', isDir: false, type: 'text', size: '12 KB', icon: 'fa-brands fa-js text-yellow-400' },
            { id: 104, name: 'cover.jpg', isDir: false, type: 'image', size: '2.1 MB', url: 'https://picsum.photos/seed/file1/300/200' },
        ]);
        const selectedFile = ref(null);
        const isEditingFile = ref(false);
        const editingFile = ref(null);
        const editingContent = ref('');
        let cmInstance = null;

        const handleFileDoubleClick = (file) => {
            if (file.type === 'text') {
                editingFile.value = file;
                editingContent.value = '// Lumine 内置编辑器\nfunction hello() {\n    console.log("Hello CodeMirror!");\n}';
                isEditingFile.value = true;
                nextTick(() => {
                    const el = document.getElementById('code-editor');
                    if (el) {
                        cmInstance = CodeMirror.fromTextArea(el, { lineNumbers: true, theme: "dracula" });
                        cmInstance.on('change', (cm) => { editingContent.value = cm.getValue(); });
                    }
                });
            }
        };

        const closeEditor = () => {
            isEditingFile.value = false;
            if (cmInstance) { cmInstance.toTextArea(); cmInstance = null; }
        };

        const saveFile = () => { alert("已保存!"); closeEditor(); };

        // ================= 媒体库数据 =================
        const photoTab = ref('albums');
        const albumsByYear = ref([{ year: '2025', albums: [{ id: 1, name: 'Test', cover: 'https://picsum.photos/seed/1/400/300', count: 10, dateRange: '2025' }] }]);
        const photosByMonth = ref([{ month: '2025年5月', items: [{ id: 1, thumb: 'https://picsum.photos/seed/2/400/500', color: '#333', aspectRatio: 0.8, type: 'image' }] }]);
        
        const mockVideos = ref([{ id: 1, name: '沙丘2', cover: 'https://picsum.photos/seed/3/600/400', size: '2GB', duration: '02:00:00', url: 'https://www.w3schools.com/html/mov_bbb.mp4', date: '2024' }]);
        const currentVideo = ref(null);
        const playVideo = (video) => { currentVideo.value = video; };

        const mockMusic = ref([{ id: 1, name: 'Song 1', artist: 'Artist', cover: 'https://picsum.photos/seed/4/200/200', url: 'https://www.soundhelix.com/examples/mp3/SoundHelix-Song-1.mp3' }]);
        const currentAudio = ref(null);
        const isPlaying = ref(false);
        const audioQueue = ref([]);
        let audioEl = new Audio();

        const playMusic = (song, queue) => {
            currentAudio.value = song;
            audioQueue.value = queue;
            isPlaying.value = true;
            audioEl.src = song.url;
            audioEl.play().catch(() => isPlaying.value = false);
        };
        const toggleAudioPlayback = () => {
            if (isPlaying.value) { audioEl.pause(); } else { audioEl.play().catch(e => console.log(e)); }
        };
        const playNext = () => { alert("Next track"); };
        const playPrev = () => { alert("Prev track"); };
        audioEl.onplay = () => isPlaying.value = true;
        audioEl.onpause = () => isPlaying.value = false;

        onMounted(() => {
            if(terminalTabs.value.length > 0) initXterm(terminalTabs.value[0].id, '~');
        });

        return {
            currentView, isSidebarCollapsed, isMobileMenuOpen, navTo,
            toggleTheme, themeText, themeIcon,
            terminalTabs, activeTerminalId, createTerminalTab, closeTerminalTab,
            showShortcuts, isCtrlActive, isAltActive, toggleStickyKey, sendTermKey,
            currentPath, directoryTree, currentFiles, selectedFile,
            isEditingFile, editingFile, editingContent, handleFileDoubleClick, closeEditor, saveFile,
            photoTab, albumsByYear, photosByMonth,
            mockVideos, currentVideo, playVideo,
            mockMusic, currentAudio, isPlaying, playMusic, toggleAudioPlayback, playNext, playPrev
        }
    }
}).mount('#app');