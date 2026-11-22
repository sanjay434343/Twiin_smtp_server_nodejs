'use strict';

const admin = require('firebase-admin');
const config = require('./config');

let _db;

function getApp() {
  if (!admin.apps.length) {
    admin.initializeApp({
      credential: admin.credential.cert({
        projectId: config.firebase.projectId,
        clientEmail: config.firebase.clientEmail,
        privateKey: config.firebase.privateKey,
      }),
    });
  }
  return admin.app();
}

function db() {
  if (!_db) {
    _db = getApp().firestore();
    _db.settings({ ignoreUndefinedProperties: true });
  }
  return _db;
}

const auth = () => getApp().auth();

module.exports = { db, auth, FieldValue: admin.firestore.FieldValue };
