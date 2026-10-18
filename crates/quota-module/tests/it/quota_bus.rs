//! A private, real JetStream checks the notice/body split without touching an
//! operator's daemon, credentials, or provider network.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use cortexkit_bus_naming::AccountNames;
use cortexkit_bus_nats::{ConnectConfig, NatsConnection};
use cortexkit_bus_testkit::{
    trust::{box_root_id, signer_root_id, system_root_id, unix_now, TrustChain},
    BusOptions, BusServer, PreloadedAccount,
};
use cortexkit_bus_trait::ContentDigest;
use quota_core::model::ProviderUsage;
use quota_module::bus::{QuotaSamples, EVENT_NAME};
use serde_json::json;
use std::{sync::Arc, time::Duration};

fn entry(at: &str) -> ProviderUsage {
    serde_json::from_value(
        json!({"provider":"deepseek", "source":"api", "fetchedAt":at,
        "usage":{"primary":{"usedPercent":25.0}},
        "accountInfo":{"email":"private@example.com", "orgName":"private org"}}),
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires nats-server >= 2.10 on PATH or CK_NATS_SERVER_BIN"]
async fn private_bus_quota_notice_and_events_get_body_round_trip() {
    tokio::time::timeout(Duration::from_secs(40), async {
        let trust = TrustChain::generate();
        let account = nkeys::KeyPair::new_account().public_key();
        let user = Arc::new(nkeys::KeyPair::new_user());
        let jwt = trust.user_jwt(&box_root_id(), &account, &user, unix_now() - 5);
        let bus = BusServer::start(BusOptions {
            preload: vec![PreloadedAccount {
                public_key: account.clone(),
                signing_keys: vec![trust.box_root_public()],
            }],
            trust: Some(trust),
            ..Default::default()
        })
        .await
        .expect("start a private NATS broker; install nats-server or set CK_NATS_SERVER_BIN");
        eprintln!("private broker: {}", bus.version);
        // Preloaded fixture accounts have no JetStream storage allowance.
        // Provision only this disposable account through NATS's system API;
        // production allowance/provisioning remains owned by ck-bus.
        let operator = &bus.trust.signer.root(&signer_root_id()).pair;
        let mut claims = json!({"jti":"", "iat":unix_now(), "iss":operator.public_key(),
            "sub":account, "name":"quota-test", "nats":{"type":"account", "version":2,
                "limits":{"subs":-1,"data":-1,"payload":-1,"imports":-1,"exports":-1,
                    "wildcards":true,"conn":-1,"leaf":-1,"mem_storage":8388608,
                    "disk_storage":8388608,"streams":-1,"consumer":-1},
                "signing_keys":[bus.trust.box_root_public()],
                "default_permissions":{"pub":{},"sub":{}}}});
        claims["jti"] = json!(ContentDigest::of_bytes(claims.to_string().as_bytes())
            .to_string()
            .to_uppercase());
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"ed25519-nkey","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
        );
        let account_jwt = format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(operator.sign(signing_input.as_bytes()).unwrap())
        );
        let system_user = Arc::new(nkeys::KeyPair::new_user());
        let system_jwt = bus.trust.user_jwt(
            &system_root_id(),
            &bus.system_account,
            &system_user,
            unix_now() - 5,
        );
        let system = async_nats::ConnectOptions::with_jwt(system_jwt, move |nonce| {
            let user = Arc::clone(&system_user);
            async move { user.sign(&nonce).map_err(async_nats::AuthError::new) }
        })
        .connect(&bus.url)
        .await
        .unwrap();
        let updated = system
            .request("$SYS.REQ.CLAIMS.UPDATE", account_jwt.into())
            .await
            .unwrap();
        let update: serde_json::Value = serde_json::from_slice(&updated.payload).unwrap();
        assert!(
            update.get("error").is_none(),
            "fixture account update refused: {update}"
        );
        let public = user.public_key();
        let options = async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
            let user = Arc::clone(&user);
            async move { user.sign(&nonce).map_err(async_nats::AuthError::new) }
        });
        let connection =
            NatsConnection::connect(&bus.url, options, ConnectConfig::new(&public).unwrap())
                .await
                .unwrap();
        let names = AccountNames::derive("box_insula_test").unwrap();
        let subject = names.event_subject("insula", EVENT_NAME, 1).unwrap();
        assert_eq!(subject, "ck.box_insula_test.event.insula.quota_sampled.v1");
        let stream = async_nats::jetstream::new(connection.client())
            .create_stream(async_nats::jetstream::stream::Config {
                name: names.streams().event.clone(),
                subjects: vec![subject.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        let samples = QuotaSamples::new("run-epoch".into());
        samples.observe(vec![entry("2026-01-01T00:00:00Z")]).await;
        assert!(samples.publish_pending(&connection, &names).await.unwrap());
        let notice = stream.get_raw_message(1).await.unwrap();
        assert_eq!(notice.subject.as_str(), subject);
        assert!(
            notice.payload.is_empty(),
            "record bodies never travel on the bus"
        );
        let headers = notice.headers;
        let id = headers.get("Nats-Msg-Id").unwrap().as_str();
        let digest = headers.get("Ck-Content-Digest").unwrap().as_str();
        assert_eq!(headers.get("kind").unwrap().as_str(), "quota_sampled");
        assert_eq!(headers.get("seq").unwrap().as_str(), "1");
        assert_eq!(headers.get("epoch").unwrap().as_str(), "run-epoch");
        assert!(headers.get("account").is_none());
        let reply = samples.events_get(id, "quota_sampled");
        let result = &reply["result"];
        assert_eq!(result["digest"], digest);
        assert_eq!(
            ContentDigest::of_bytes(&serde_json::to_vec(&result["body"]).unwrap()).to_string(),
            digest
        );
        let body = &result["body"];
        assert_eq!(body["provider"], "deepseek");
        assert_eq!(body["usage"]["primary"]["usedPercent"], 25.0);
        assert_eq!(body["epoch"], "run-epoch");
        assert_eq!(body["seq"], 1);
        assert!(body.get("accountInfo").is_none());
        assert!(!serde_json::to_string(body)
            .unwrap()
            .contains("private@example.com"));
        samples.observe(vec![entry("2026-01-01T00:00:00Z")]).await;
        assert!(
            !samples.publish_pending(&connection, &names).await.unwrap(),
            "unchanged turn must not publish"
        );
        assert!(stream.get_raw_message(2).await.is_err());
        samples.observe(vec![entry("2026-01-01T00:01:00Z")]).await;
        assert!(samples.publish_pending(&connection, &names).await.unwrap());
        let second = stream.get_raw_message(2).await.unwrap();
        let headers = second.headers;
        assert_eq!(headers.get("seq").unwrap().as_str(), "2");
        let second_key = headers.get("Nats-Msg-Id").unwrap().as_str();
        assert_ne!(id, second_key);
        assert_eq!(
            samples.events_get(second_key, "quota_sampled")["result"]["body"]["seq"],
            2
        );
        drop(connection);
        bus.stop().await;
    })
    .await
    .expect("private bus integration deadline");
}
