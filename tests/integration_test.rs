//! Real Anvil ERC-20 authorization, replay and settlement journey.
use alloy::{network::EthereumWallet, providers::ProviderBuilder, sol};
use alloy::{
    primitives::{Address, U256},
    signers::local::PrivateKeySigner,
};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};
use tangle_inference_core::payment::{DirectProvider, PaymentProof, PaymentProvider};

const TEST_OPERATOR_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const CALLER_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
// Minimal ERC-20 compiled from tests/TestERC20.sol with solc --optimize
sol! {
    #[sol(rpc, bytecode = "6080604052348015600e575f80fd5b506102da8061001c5f395ff3fe608060405234801561000f575f80fd5b506004361061003f575f3560e01c806340c10f191461004357806370a0823114610058578063a9059cbb1461008a575b5f80fd5b610056610051366004610222565b6100ad565b005b61007761006636600461024a565b5f6020819052908152604090205481565b6040519081526020015b60405180910390f35b61009d610098366004610222565b61011d565b6040519015158152602001610081565b6001600160a01b0382165f90815260208190526040812080548392906100d490849061027e565b90915550506040518181526001600160a01b038316905f907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a35050565b335f9081526020819052604081205482111561016e5760405162461bcd60e51b815260206004820152600c60248201526b1a5b9cdd59999a58da595b9d60a21b604482015260640160405180910390fd5b335f908152602081905260408120805484929061018c908490610291565b90915550506001600160a01b0383165f90815260208190526040812080548492906101b890849061027e565b90915550506040518281526001600160a01b0384169033907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a35060015b92915050565b80356001600160a01b038116811461021d575f80fd5b919050565b5f8060408385031215610233575f80fd5b61023c83610207565b946020939093013593505050565b5f6020828403121561025a575f80fd5b61026382610207565b9392505050565b634e487b7160e01b5f52601160045260245ffd5b808201808211156102015761020161026a565b818103818111156102015761020161026a56fea2646970667358221220001234497cefa9f349ddcdf662ebeb0ca10ba368b3f04c0bbfa52d9fa39db83d64736f6c634300081a0033")]
    contract TestToken {
        function mint(address to, uint256 amount) external;
        function transfer(address to, uint256 amount) external returns (bool);
        function balanceOf(address owner) external view returns (uint256);
    }
}

struct AnvilInstance {
    child: Child,
    port: u16,
}

impl AnvilInstance {
    fn spawn() -> Self {
        // Use port 0 → Anvil picks a free port. We parse it from stdout.
        // But Anvil --silent suppresses that, so we bind a TcpListener
        // to port 0 first, get the port, close it, then give it to Anvil.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let child = Command::new("anvil")
            .args(["--port", &port.to_string(), "--silent"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("anvil must be installed (foundry)");

        // Wait for it to be ready
        std::thread::sleep(Duration::from_millis(800));
        AnvilInstance { child, port }
    }

    fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for AnvilInstance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn direct_transfer_authorization_and_rejections_on_anvil() -> anyhow::Result<()> {
    if std::env::var("ANVIL_E2E").as_deref() != Ok("1") {
        eprintln!("set ANVIL_E2E=1 to run the chain journey");
        return Ok(());
    }
    let anvil = AnvilInstance::spawn();
    let rpc = anvil.rpc_url();
    let operator: PrivateKeySigner = TEST_OPERATOR_KEY.parse()?;
    let recipient = operator.address();
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(operator))
        .connect_http(rpc.parse()?);
    let token = TestToken::deploy(&deployer).await?;
    let token_address = *token.address();
    let caller: PrivateKeySigner = CALLER_KEY.parse()?;
    let sender = caller.address();
    token
        .mint(sender, U256::from(1_000_000))
        .send()
        .await?
        .get_receipt()
        .await?;
    assert_eq!(token.balanceOf(sender).call().await?, U256::from(1_000_000));
    let payer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(caller))
        .connect_http(rpc.parse()?);
    let payer_token = TestToken::new(token_address, &payer);
    let provider = DirectProvider::new(
        rpc,
        TEST_OPERATOR_KEY.into(),
        Some(format!("{token_address:#x}")),
        0,
        None,
    )?;
    for (destination, sent, claimed, expected_error) in [
        (recipient, 500_000, 500_000, None),
        (recipient, 100, 500, Some("less than requested")),
        (
            "0x000000000000000000000000000000000000dEaD".parse::<Address>()?,
            500,
            500,
            Some("no ERC-20 Transfer to operator"),
        ),
    ] {
        let receipt = payer_token
            .transfer(destination, U256::from(sent))
            .send()
            .await?
            .get_receipt()
            .await?;
        let proof = PaymentProof::DirectTransfer {
            tx_hash: format!("{:#x}", receipt.transaction_hash),
            from: format!("{sender:#x}"),
            amount: claimed.to_string(),
            token: format!("{token_address:#x}"),
        };
        match expected_error {
            Some(message) => assert!(provider
                .authorize(&proof)
                .await
                .unwrap_err()
                .to_string()
                .contains(message)),
            None => {
                assert_eq!(provider.authorize(&proof).await?, claimed);
                provider.settle(&proof, 400_000).await?;
                assert!(provider
                    .authorize(&proof)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("replay"));
            }
        }
    }
    let missing = PaymentProof::DirectTransfer {
        tx_hash: format!("0x{:064x}", 1),
        from: format!("{sender:#x}"),
        amount: "100".into(),
        token: format!("{token_address:#x}"),
    };
    assert!(provider
        .authorize(&missing)
        .await
        .unwrap_err()
        .to_string()
        .contains("not found"));
    Ok(())
}
