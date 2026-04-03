/**
 * Ozma Mobile — entry point
 * https://github.com/ozmalabs/ozma
 */

import {AppRegistry} from 'react-native';
import App from './App';
import {name as appName} from './app.json';

// FCM background message handler must be registered before any other code
import {PushManager} from './src/push/PushManager';
PushManager.registerBackgroundHandler();

AppRegistry.registerComponent(appName, () => App);
