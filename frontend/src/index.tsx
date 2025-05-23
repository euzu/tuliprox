import React from 'react';
import {createRoot} from 'react-dom/client';
import './index.scss';
import {SnackbarProvider} from 'notistack';
import {ServiceProvider} from "./provider/service-provider";
import Authentication from "./component/authentication/authentication";
import Fetcher from "./utils/fetcher";
import ServiceContext from "./service/service-context";
import {UiConfig} from './model/ui-config';
import i18n_init from "./utils/i18n";

import {catchError, map, switchMap} from 'rxjs/operators';
import {EMPTY} from 'rxjs';
import Tooltip from "./component/tooltip/tooltip";

const initUI = () => {
    const container = document.getElementById('root');
    const root = createRoot(container);
    root.render(
        <SnackbarProvider maxSnack={3} autoHideDuration={1500} anchorOrigin={({vertical: 'top', horizontal: 'center'})}>
            <Tooltip></Tooltip>
            <ServiceProvider>
                <Authentication/>
            </ServiceProvider>
        </SnackbarProvider>
    );
}

Fetcher.fetchJson("config.json").pipe(
    switchMap((config: UiConfig) => {
            ServiceContext.config().setUiConfig(config);
            const document_title = config.tab_title || config.app_title;
            if (document_title) {
                document.title = document_title;
            }
            return i18n_init(ServiceContext.config().getUiConfig().languages).pipe(
                map(() => {
                    initUI();
                })
            )
        }
    ),
    catchError((error: Error) => {
        initUI();
        return EMPTY;
    })
).subscribe();

// Fetcher.fetchJson("config.json").subscribe({
//     next: (config: UiConfig) => {
//         i18n_init(config.languages).subscribe(
//         ServiceContext.config().setUiConfig(config);
//         initUI();
//     },
//     error: (error: Error) => {initUI()}
// });

