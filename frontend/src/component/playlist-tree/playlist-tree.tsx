import React, {useCallback, useState, useRef, useEffect} from 'react';
import './playlist-tree.scss';
import {PlaylistCategories, PlaylistGroup, PlaylistItem} from "../../model/playlist";
import copyToClipboard from "../../utils/clipboard";
import {first} from "rxjs/operators";
import {noop} from "rxjs";
import {useSnackbar} from "notistack";
import {getIconByName} from "../../icons/icons";
import ServerConfig from "../../model/server-config";
import {PlaylistRequest, PlaylistRequestType} from "../../model/playlist-request";

export type PlaylistTreeState = { [key: number]: boolean };

interface PlaylistTreeProps {
    serverConfig: ServerConfig;
    data: PlaylistCategories;
    playlistRequest: PlaylistRequest;
    state: PlaylistTreeState;
    onCopy: (playlistItem: PlaylistItem) => void;
    onPlay?: (playlistItem: PlaylistItem) => void;
    onDownload?: (playlistItem: PlaylistItem) => void;
    onWebSearch?: (playlistItem: PlaylistItem) => void;
}

export default function PlaylistTree(props: PlaylistTreeProps) {
    const {serverConfig, data, playlistRequest, onCopy, onPlay, onDownload, onWebSearch} = props;

    const [, setForceUpdate] = useState(undefined);
    const expanded = useRef<PlaylistTreeState>({});
    const {enqueueSnackbar/*, closeSnackbar*/} = useSnackbar();
    const [videoExtensions, setVideoExtensions] = useState<string[]>([]);

    useEffect(() => {
        if (serverConfig) {
            setVideoExtensions(serverConfig.video?.extensions);
        }
        return noop;
    }, [serverConfig]);

    const getPlaylistItemById = useCallback((itemId: string): PlaylistItem => {
        const id = parseInt(itemId);
        if (data && !isNaN(id)) {
            const groups = [data?.live, data?.vod, data?.series].filter(Boolean).flat();
            for (let i = 0, len = groups.length; i < len; i++) {
                const group = groups[i];
                for (let j = 0, clen = group.channels?.length ?? 0; j < clen; j++) {
                    const plitem = group.channels[j];
                    // eslint-disable-next-line eqeqeq
                    if (plitem.id == id) {
                        return plitem;
                    }
                }
            }
        }
        return undefined;
    }, [data]);

    const handleExpand = useCallback((event: any) => {
        const key = event.target.dataset.group;
        expanded.current[key] = !expanded.current[key];
        setForceUpdate({});
    }, []);

    const handleClipboardRealUrl = useCallback((e: any) => {
        const item = getPlaylistItemById(e.target.dataset.item);
        if (item) {
            let virtual_id = undefined;
            switch (playlistRequest.rtype) {
                case PlaylistRequestType.TARGET:
                    virtual_id = item.virtual_id;
                    break;
                case PlaylistRequestType.INPUT:
                case PlaylistRequestType.XTREAM:
                case PlaylistRequestType.M3U:
                    break;
            }
            if (virtual_id) {
                onCopy(item);
                copyToClipboard(''+virtual_id).pipe(first()).subscribe({
                    next: value => enqueueSnackbar(value ? "URL copied to clipboard" : "Copy to clipboard failed!", {variant: value ? 'success' : 'error'}),
                    error: _ => enqueueSnackbar("Copy to clipboard failed!", {variant: 'error'}),
                    complete: noop,
                });
            }            
        }
    }, [enqueueSnackbar, getPlaylistItemById, onCopy, playlistRequest]);

    const handleClipboardUrl = useCallback((e: any) => {
        const item = getPlaylistItemById(e.target.dataset.item);
        if (item) {


            onCopy(item);
            copyToClipboard(item.url).pipe(first()).subscribe({
                next: value => enqueueSnackbar(value ? "URL copied to clipboard" : "Copy to clipboard failed!", {variant: value ? 'success' : 'error'}),
                error: _ => enqueueSnackbar("Copy to clipboard failed!", {variant: 'error'}),
                complete: noop,
            });
        }
    }, [enqueueSnackbar, getPlaylistItemById, onCopy]);

    const handleWebSearch = useCallback((e: any) => {
        if (onWebSearch) {
            const item = getPlaylistItemById(e.target.dataset.item);
            if (item) {
                onWebSearch(item);
            }
        }
   }, [getPlaylistItemById, onWebSearch]);

    const handleDownloadUrl = useCallback((e: any) => {
        if (onDownload) {
            if (!serverConfig.video.download?.directory) {
                enqueueSnackbar("Please updated the server configuration and add video.download directory and headers!", {variant: 'error'})
            } else {
                const item = getPlaylistItemById(e.target.dataset.item);
                if (item) {
                    onDownload(item);
                }
            }
        }
    }, [serverConfig, enqueueSnackbar, getPlaylistItemById, onDownload]);

    const handlePlayUrl = useCallback((e: any) => {
        if (onPlay) {
            const item = getPlaylistItemById(e.target.dataset.item);
            if (item) {
                onPlay(item);
            }
        }
    }, [onPlay, getPlaylistItemById]);

    const isVideoFile = useCallback((entry: PlaylistItem): boolean => {
        if (videoExtensions && entry.url) {
            for (const ext of videoExtensions) {
                if (entry.url.endsWith(ext)) {
                    return true;
                }
            }
        }
        return false;
    }, [videoExtensions]);

    const renderEntry = useCallback((entry: PlaylistItem, index: number): React.ReactNode => {
        return <div key={entry.id} className={'tree-group__channel'}>
            <div className={'tree-group__channel-tools'}>
                <div className={'tool-button'} data-item={entry.id} onClick={handleClipboardUrl}>
                    {getIconByName('LinkRounded')}
                </div>
                <div className={'tool-button'} data-item={entry.id} onClick={handleClipboardRealUrl}>
                    {getIconByName('LinkRounded')}
                </div>
                {onPlay && <div className={'tool-button'} data-item={entry.id} onClick={handlePlayUrl}>
                    {getIconByName('PlayArrow')}
                </div>}
                {isVideoFile(entry) &&
                    <>
                        <div className={'tool-button'} data-item={entry.id} onClick={handleDownloadUrl}>
                            {getIconByName('Download')}
                        </div>
                        {serverConfig.video?.web_search &&
                            <div className={'tool-button'} data-item={entry.id} onClick={handleWebSearch}>
                                {getIconByName('WebSearch')}
                            </div>
                        }
                    </>
                }
            </div>
            <div className={'tree-group__channel-content'}>
                <div className={'tree-group__channel-nr'}>{index + 1}</div>
                {entry.name}</div>
        </div>
    }, [onPlay, handleClipboardUrl, handlePlayUrl, handleDownloadUrl, isVideoFile, handleWebSearch, serverConfig, handleClipboardRealUrl]);

    const renderGroup = useCallback((group: PlaylistGroup): React.ReactNode => {
        return <div className={'tree-group'} key={group.id}>
            <div className={'tree-group__header'}>
                <div className={'tree-expander'} data-group={group.id}
                     onClick={handleExpand}>{getIconByName(expanded.current[group.id] ?
                    'ExpandMore' : 'ChevronRight')}</div>
                <div className={'tree-group__header-content'}>
                    {group.name}
                    <div className={'tree-group__count'}>({group.channels?.length})</div>
                </div>
            </div>
            {expanded.current[group.id] && (
                <div className={'tree-group__childs'}>
                    {group.channels?.map(renderEntry)}
                </div>)}
        </div>;
    }, [handleExpand, renderEntry]);

    const renderPlaylist = useCallback((): React.ReactNode => {
        if (!data) {
            return <React.Fragment/>;
        }
        return <React.Fragment>
            {[data?.live, data?.vod, data?.series].filter(Boolean).flat().map(renderGroup)}
        </React.Fragment>;
    }, [data, renderGroup]);

    return <div className={'playlist-tree'}>{renderPlaylist()}</div>;
} 